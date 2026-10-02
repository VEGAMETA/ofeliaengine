//! Textures of materials (ADR 0040, ADR 0057): images with full mip chains, uploaded
//! within a budget of bytes per frame, and registered in the bindless set as their levels
//! arrive. Until then materials shade without them (their factors alone), so a scene
//! appears at once and its textures fill in over the next frames.
//!
//! Two ways in:
//!
//! - [`TexturePool::add`] takes every level in memory and uploads them together (through a
//!   staging buffer made for the frame).
//! - **Streaming:** [`TexturePool::add_empty`] makes the image,
//!   [`TexturePool::staging`] lends memory of the pool's staging ring (host-cached, so
//!   drive reads land in it and decompression writes into it directly), and
//!   [`TexturePool::upload_levels`] copies any range of levels from there. A texture is
//!   resident from its smallest level up: each time a larger level joins that range it
//!   gets a view over the resident levels and a new bindless index (materials pick it up
//!   through [`TexturePool::generation`]); the old view is freed once the frames that used
//!   it have finished.
//! - **Residency that grows and shrinks** (ADR 0076): [`TexturePool::add_streamed`] makes
//!   an image that holds only the levels from a first one down, and
//!   [`TexturePool::set_base`] moves that first level: the pool makes an image of the new
//!   size, copies the resident levels the two share on the GPU, and retires the old one
//!   once the frames using it have finished. Growing makes room for finer levels before
//!   they arrive; shrinking evicts the finest levels and returns their memory. Copies
//!   count against the same bytes-per-frame budget as uploads, so a frame never spends
//!   more on texture traffic than that (except for a single upload larger than it).
//!   [`TexturePool::memory`] reports the images' bytes for the GPU memory budget
//!   ([`crate::budget`]).
//!
//! Uploads record their own layout transitions: materials sample textures through
//! bindless indices without declaring them to the graph, so a level is either being
//! uploaded (inside the upload pass) or read-only.

use crate::graph::{Access, BufferState, FrameGraph, ImportedBuffer};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError};
use std::collections::VecDeque;

/// A texture in a [`TexturePool`]. Ids of removed textures are stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId {
    index: u32,
    generation: u32,
}

/// How a texture's texels are stored (ADR 0041).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextureFormat {
    /// 4 bytes per texel, sRGB color (the view decodes it).
    Rgba8Srgb,
    /// 4 bytes per texel, linear data.
    Rgba8Unorm,
    /// BC7 blocks (16 bytes per 4 × 4 texels), sRGB color.
    Bc7Srgb,
    /// BC7 blocks, linear data.
    Bc7Unorm,
    /// BC5 blocks: red and green only (normal maps; blue reads 0, alpha 1).
    Bc5Unorm,
}

impl TextureFormat {
    /// Bytes of a `width × height` level.
    pub fn level_bytes(self, width: u32, height: u32) -> u64 {
        match self {
            Self::Rgba8Srgb | Self::Rgba8Unorm => u64::from(width) * u64::from(height) * 4,
            Self::Bc7Srgb | Self::Bc7Unorm | Self::Bc5Unorm => {
                u64::from(width.div_ceil(4)) * u64::from(height.div_ceil(4)) * 16
            }
        }
    }

    fn vk(self) -> vk::Format {
        match self {
            Self::Rgba8Srgb => vk::Format::R8G8B8A8_SRGB,
            Self::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
            Self::Bc7Srgb => vk::Format::BC7_SRGB_BLOCK,
            Self::Bc7Unorm => vk::Format::BC7_UNORM_BLOCK,
            Self::Bc5Unorm => vk::Format::BC5_UNORM_BLOCK,
        }
    }
}

/// A texture to add: levels from the full size down, each `max(1, size >> level)` on a
/// side, rows (of texels or of blocks) tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextureLevels {
    /// Width of level 0.
    pub width: u32,
    /// Height of level 0.
    pub height: u32,
    /// How texels are stored.
    pub format: TextureFormat,
    /// The levels, at least one.
    pub levels: Vec<Vec<u8>>,
}

/// Staging offsets of levels are multiples of this: the largest texel block (16 bytes).
/// In a [`StagingSpan`], levels follow one another, each padded to it.
pub const LEVEL_ALIGN: u64 = 16;
/// Staging spans start at multiples of this, so unbuffered drive reads land in them.
pub const STAGING_ALIGN: u64 = 4096;
/// The staging ring's size unless [`TexturePool::set_staging_size`] says otherwise: room
/// for a 4K texture's top level (16 MiB of BC7) four times over.
pub const DEFAULT_STAGING_BYTES: u64 = 64 << 20;
/// Most levels a texture may have (a 2^31-texel side).
const MAX_LEVELS: u32 = 32;

/// Why a texture or an upload was not accepted.
#[derive(Debug)]
pub enum TextureError {
    /// No levels, a level whose size does not match its dimensions, or levels outside the
    /// texture.
    BadLevels,
    /// Asked for more staging memory than the ring holds.
    StagingTooSmall,
    /// A Vulkan call failed.
    Rhi(RhiError),
}

impl From<RhiError> for TextureError {
    fn from(e: RhiError) -> Self {
        Self::Rhi(e)
    }
}

impl core::fmt::Display for TextureError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadLevels => write!(f, "texture levels do not match its size"),
            Self::StagingTooSmall => write!(f, "larger than the staging ring"),
            Self::Rhi(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TextureError {}

/// Staging memory lent by [`TexturePool::staging`]: fill it (drive reads, decompression,
/// from any thread), then hand it back with [`TexturePool::upload_levels`] or
/// [`TexturePool::cancel_staging`].
#[derive(Debug)]
pub struct StagingSpan {
    /// Position in the ring (monotonic; the ring offset is modulo its size).
    start: u64,
    offset: u64,
    len: u64,
    address: *mut u8,
}

// SAFETY: the span is the only handle to its range of the ring; filling it from another
// thread is its purpose.
unsafe impl Send for StagingSpan {}

impl StagingSpan {
    /// Its first byte (a multiple of [`STAGING_ALIGN`]). Valid for writes of
    /// [`Self::len`] bytes until the span is handed back, while the pool lives.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.address
    }

    /// Its length (a multiple of [`STAGING_ALIGN`]).
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Never empty (kept for the `len` convention).
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Its bytes.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the ring's mapped memory holds `len` bytes at `address`, lent to this
        // span alone until it is handed back.
        unsafe { core::slice::from_raw_parts_mut(self.address, self.len as usize) }
    }
}

/// Where spans of the staging ring are: lent in order, given back in any order; memory is
/// reused once every older span has come back (and the frames copying from it have
/// finished).
#[derive(Debug, Default)]
struct RingSpans {
    size: u64,
    /// Next free position (monotonic; ring offsets are modulo `size`).
    head: u64,
    /// Lent spans in order: start, length, back.
    spans: VecDeque<(u64, u64, bool)>,
}

impl RingSpans {
    fn tail(&self) -> u64 {
        self.spans.front().map_or(self.head, |s| s.0)
    }

    /// A span of at least `bytes`: its start (monotonic), offset and length.
    fn lend(&mut self, bytes: u64) -> Option<(u64, u64, u64)> {
        let len = bytes.max(1).next_multiple_of(STAGING_ALIGN);
        if self.spans.is_empty() {
            // Nothing lent: start again at the beginning of the ring.
            self.head = self.head.next_multiple_of(self.size);
        }
        let mut start = self.head;
        if start % self.size + len > self.size {
            // Skip the end of the ring: spans are contiguous.
            start = start.next_multiple_of(self.size);
        }
        if start + len - self.tail() > self.size {
            return None;
        }
        self.spans.push_back((start, len, false));
        self.head = start + len;
        Some((start, start % self.size, len))
    }

    fn give_back(&mut self, start: u64) {
        if let Some(span) = self.spans.iter_mut().find(|s| s.0 == start) {
            span.2 = true;
        }
        while self.spans.front().is_some_and(|s| s.2) {
            self.spans.pop_front();
        }
    }
}

/// The streaming staging ring: one host-cached buffer, lent out in spans.
struct StagingRing {
    buffer: vk::Buffer,
    allocation: Allocation,
    /// The ring's first byte: the buffer's first page-aligned address.
    base: *mut u8,
    /// Where the ring starts in the buffer.
    skip: u64,
    spans: RingSpans,
}

impl StagingRing {
    fn lend(&mut self, bytes: u64) -> Option<StagingSpan> {
        let (start, offset, len) = self.spans.lend(bytes)?;
        Some(StagingSpan {
            start,
            offset: self.skip + offset,
            len,
            // SAFETY: `offset + len <= size`, within the mapped buffer.
            address: unsafe { self.base.add(offset as usize) },
        })
    }

    fn give_back(&mut self, start: u64) {
        self.spans.give_back(start);
    }
}

#[derive(Debug)]
struct PoolTexture {
    image: vk::Image,
    allocation: Option<Allocation>,
    format: TextureFormat,
    width: u32,
    height: u32,
    levels: u32,
    /// The first level the image holds: its mip `i` is the texture's level `base + i`.
    base: u32,
    /// A first level asked for by [`TexturePool::set_base`], not made yet.
    target: Option<u32>,
    /// Levels uploaded (texture levels, one bit each; only `base..levels` can be).
    uploaded: u32,
    /// First level of the view: the resident levels are `resident..levels`.
    resident: u32,
    /// View over the resident levels.
    view: Option<vk::ImageView>,
    /// Its bindless index.
    bindless: Option<u32>,
}

impl PoolTexture {
    fn level_size(&self, level: u32) -> (u32, u32) {
        ((self.width >> level).max(1), (self.height >> level).max(1))
    }

    /// Staging bytes of `count` levels from `first`, each padded to [`LEVEL_ALIGN`].
    fn staging_bytes(&self, first: u32, count: u32) -> u64 {
        (first..first + count)
            .map(|k| {
                let (w, h) = self.level_size(k);
                self.format.level_bytes(w, h).next_multiple_of(LEVEL_ALIGN)
            })
            .sum()
    }

    /// The first level the image will hold once pending changes are made.
    fn planned_base(&self) -> u32 {
        self.target.unwrap_or(self.base)
    }

    /// The image's bytes in the GPU's memory.
    fn bytes(&self) -> u64 {
        self.allocation.as_ref().map_or(0, Allocation::size)
    }
}

/// A texture's numbers ([`TexturePool::info`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureInfo {
    /// Width of level 0.
    pub width: u32,
    /// Height of level 0.
    pub height: u32,
    /// Levels of the full texture.
    pub levels: u32,
    /// How texels are stored.
    pub format: TextureFormat,
    /// The first level the image holds (0: all of them).
    pub base: u32,
    /// The first level it will hold once a pending [`TexturePool::set_base`] is made.
    pub planned_base: u32,
    /// The first resident level (`levels` while none is).
    pub resident: u32,
    /// Levels uploaded, one bit each.
    pub uploaded: u32,
    /// The image's bytes in GPU memory.
    pub bytes: u64,
}

/// Texel bytes of the levels `base..levels` of a texture: what an image holding them
/// stores, before the driver's alignment (for planning; [`TexturePool::memory`] reports
/// the real sizes).
pub fn levels_bytes(format: TextureFormat, width: u32, height: u32, levels: u32, base: u32) -> u64 {
    (base..levels)
        .map(|k| format.level_bytes((width >> k).max(1), (height >> k).max(1)))
        .sum()
}

#[derive(Debug)]
struct Slot {
    generation: u32,
    texture: Option<PoolTexture>,
}

/// Where an upload's data is.
#[derive(Debug)]
enum Source {
    /// In memory: copied into a staging buffer made for the frame.
    Owned(Vec<Vec<u8>>),
    /// In the staging ring.
    Span(StagingSpan),
}

#[derive(Debug)]
struct Upload {
    id: TextureId,
    first: u32,
    count: u32,
    bytes: u64,
    source: Source,
}

#[derive(Debug)]
enum Retired {
    Texture(PoolTexture),
    /// An image replaced by one of another size, with its view.
    Image(vk::Image, Allocation, Option<vk::ImageView>),
    View(vk::ImageView),
    Staging(vk::Buffer, Allocation),
    /// A ring span (its start).
    Span(u64),
}

/// A reallocation recorded in a frame: the old image's levels copied into the new one.
struct Move {
    old: vk::Image,
    new: vk::Image,
    /// `(old mip, new mip, width, height)` of each copied level.
    levels: [(u32, u32, u32, u32); MAX_LEVELS as usize],
    count: u32,
}

/// Material textures (see the module documentation).
pub struct TexturePool {
    device: engine_rhi::ash::Device,
    slots: Vec<Slot>,
    free_slots: Vec<u32>,
    queue: VecDeque<Upload>,
    /// Textures with a pending [`Self::set_base`], oldest first.
    moves: VecDeque<TextureId>,
    budget: u64,
    retired: Vec<(u64, Retired)>,
    frames_in_flight: u64,
    /// Changes whenever textures become ready or are removed: materials resolve their
    /// textures again when it does.
    generation: u64,
    uploaded_bytes: u64,
    /// Bytes copied by reallocations so far.
    moved_bytes: u64,
    /// Bytes of texture traffic (uploads and copies) recorded in the last frame.
    frame_bytes: u64,
    /// Bytes of the live textures' images, and of retired images not freed yet.
    live_bytes: u64,
    retiring_bytes: u64,
    ring: Option<StagingRing>,
    ring_bytes: u64,
}

impl core::fmt::Debug for TexturePool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TexturePool")
            .field("textures", &(self.slots.len() - self.free_slots.len()))
            .field("queued", &self.queue.len())
            .field("budget", &self.budget)
            .field("uploaded_bytes", &self.uploaded_bytes)
            .field("live_bytes", &self.live_bytes)
            .field("staging_ring", &self.ring.as_ref().map(|r| r.spans.size))
            .finish()
    }
}

impl TexturePool {
    /// An empty pool uploading at most `budget` bytes per frame (at least one upload,
    /// however large); frames in flight bound how long removed textures are kept.
    pub fn new(device: &Device, budget: u64, frames_in_flight: usize) -> Self {
        Self {
            device: device.raw().clone(),
            slots: Vec::new(),
            free_slots: Vec::new(),
            queue: VecDeque::new(),
            moves: VecDeque::new(),
            budget,
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            generation: 0,
            uploaded_bytes: 0,
            moved_bytes: 0,
            frame_bytes: 0,
            live_bytes: 0,
            retiring_bytes: 0,
            ring: None,
            ring_bytes: DEFAULT_STAGING_BYTES,
        }
    }

    /// The staging ring's size, for streaming ([`DEFAULT_STAGING_BYTES`] by default):
    /// set before the first [`Self::staging`]. It bounds the largest single upload and how
    /// much can be read ahead.
    pub fn set_staging_size(&mut self, bytes: u64) {
        if self.ring.is_none() {
            self.ring_bytes = bytes.max(STAGING_ALIGN).next_multiple_of(STAGING_ALIGN);
        }
    }

    /// The bytes of uploads and copies recorded per frame.
    pub fn frame_budget(&self) -> u64 {
        self.budget
    }

    /// Changes the bytes of uploads and copies recorded per frame (at least one is
    /// recorded every frame, however large).
    pub fn set_frame_budget(&mut self, bytes: u64) {
        self.budget = bytes;
    }

    /// Creates a texture's image now and queues its levels for upload.
    pub fn add(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        texture: TextureLevels,
    ) -> Result<TextureId, TextureError> {
        let _ = device;
        let levels = texture.levels.len() as u32;
        let (width, height) = (texture.width.max(1), texture.height.max(1));
        let mut bytes = 0;
        for (k, level) in texture.levels.iter().enumerate() {
            let size = |s: u32| (s >> k).max(1);
            let expected = texture.format.level_bytes(size(width), size(height));
            if level.len() as u64 != expected {
                return Err(TextureError::BadLevels);
            }
            bytes += expected.next_multiple_of(LEVEL_ALIGN);
        }
        let id = self.add_empty(allocator, width, height, texture.format, levels)?;
        self.queue.push_back(Upload {
            id,
            first: 0,
            count: levels,
            bytes,
            source: Source::Owned(texture.levels),
        });
        Ok(id)
    }

    /// Creates a texture's image of `levels` levels with no data: upload them with
    /// [`Self::upload_levels`]. Materials see it once its smallest level has arrived.
    pub fn add_empty(
        &mut self,
        allocator: &mut Allocator,
        width: u32,
        height: u32,
        format: TextureFormat,
        levels: u32,
    ) -> Result<TextureId, TextureError> {
        self.add_streamed(allocator, width, height, format, levels, 0)
    }

    /// Creates a texture of `levels` levels whose image holds only the levels from `base`
    /// down (the rest come with [`Self::set_base`]), with no data: upload them with
    /// [`Self::upload_levels`].
    pub fn add_streamed(
        &mut self,
        allocator: &mut Allocator,
        width: u32,
        height: u32,
        format: TextureFormat,
        levels: u32,
        base: u32,
    ) -> Result<TextureId, TextureError> {
        let (width, height) = (width.max(1), height.max(1));
        let full = 32 - width.max(height).leading_zeros();
        if levels == 0 || levels > full.min(MAX_LEVELS) || base >= levels {
            return Err(TextureError::BadLevels);
        }
        let (image, allocation) = create_image(allocator, width, height, format, levels, base)?;
        self.live_bytes += allocation.size();
        let entry = PoolTexture {
            image,
            allocation: Some(allocation),
            format,
            width,
            height,
            levels,
            base,
            target: None,
            uploaded: 0,
            resident: levels,
            view: None,
            bindless: None,
        };
        let index = match self.free_slots.pop() {
            Some(index) => index,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    texture: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.texture = Some(entry);
        Ok(TextureId {
            index,
            generation: slot.generation,
        })
    }

    /// Makes the texture's image hold the levels from `base` down (ADR 0076), in the next
    /// [`Self::begin_frame`] that has room in its budget: a new image of that size, the
    /// uploaded levels the two share copied into it. A lower `base` makes room for finer
    /// levels (upload them once this returns); a higher one evicts the finest levels.
    /// Uploads queued for levels the new image does not hold are dropped. Asking for the
    /// current first level cancels a pending change.
    pub fn set_base(&mut self, id: TextureId, base: u32) -> Result<(), TextureError> {
        let Some(t) = self.texture_mut(id) else {
            return Err(TextureError::BadLevels);
        };
        if base >= t.levels {
            return Err(TextureError::BadLevels);
        }
        let queued = t.target.is_some();
        if base == t.base {
            t.target = None;
        } else {
            t.target = Some(base);
            if !queued {
                self.moves.push_back(id);
            }
        }
        Ok(())
    }

    /// Lends `bytes` of the staging ring (made at the first call, in host-cached memory),
    /// or `None` while it is full: hand spans back and try again next frame.
    pub fn staging(
        &mut self,
        allocator: &mut Allocator,
        bytes: u64,
    ) -> Result<Option<StagingSpan>, TextureError> {
        if bytes > self.ring_bytes {
            return Err(TextureError::StagingTooSmall);
        }
        if self.ring.is_none() {
            let size = self.ring_bytes;
            // One block more: the ring starts at the first page-aligned address, wherever
            // the allocator placed the buffer.
            let (buffer, mut allocation) = allocator.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size + STAGING_ALIGN)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                MemoryLocation::Streaming,
            )?;
            let mapped = allocation
                .mapped_slice_mut()
                .map(|m| (m.as_mut_ptr(), m.len() as u64));
            let Some((address, _)) = mapped.filter(|m| m.1 >= size + STAGING_ALIGN) else {
                // SAFETY: never used.
                unsafe { allocator.destroy_buffer(buffer, allocation) };
                return Err(RhiError::Vulkan {
                    call: "staging ring not mapped",
                    result: vk::Result::ERROR_MEMORY_MAP_FAILED,
                }
                .into());
            };
            let skip = (address as u64).next_multiple_of(STAGING_ALIGN) - address as u64;
            self.ring = Some(StagingRing {
                buffer,
                allocation,
                // SAFETY: `skip < STAGING_ALIGN`, within the mapping.
                base: unsafe { address.add(skip as usize) },
                skip,
                spans: RingSpans {
                    size,
                    ..RingSpans::default()
                },
            });
        }
        Ok(self.ring.as_mut().and_then(|ring| ring.lend(bytes)))
    }

    /// Hands back a span without uploading from it.
    pub fn cancel_staging(&mut self, span: StagingSpan) {
        if let Some(ring) = self.ring.as_mut() {
            ring.give_back(span.start);
        }
    }

    /// Queues the upload of `count` levels from `first` out of `span`, where they follow
    /// one another, each padded to [`LEVEL_ALIGN`] (the layout of a cooked pack's texture
    /// entry). The span comes back to the ring once the frame that copies it has finished.
    /// A stale id or levels outside the texture (or below the first level its image holds
    /// or will hold, [`Self::set_base`]) give the span back and fail.
    pub fn upload_levels(
        &mut self,
        id: TextureId,
        first: u32,
        count: u32,
        span: StagingSpan,
    ) -> Result<(), TextureError> {
        let bytes = self.texture(id).and_then(|t| {
            let fits = count > 0
                && first >= t.planned_base()
                && first.checked_add(count).is_some_and(|end| end <= t.levels);
            fits.then(|| t.staging_bytes(first, count))
        });
        match bytes {
            Some(bytes) if bytes <= span.len => {
                self.queue.push_back(Upload {
                    id,
                    first,
                    count,
                    bytes,
                    source: Source::Span(span),
                });
                Ok(())
            }
            _ => {
                self.cancel_staging(span);
                Err(TextureError::BadLevels)
            }
        }
    }

    /// Removes a texture: materials that use it shade without it from the next
    /// [`crate::material::MaterialTable::begin_frame`] on; its memory is freed once the
    /// frames up to `frame_number` (the current one) have finished.
    pub fn remove(&mut self, id: TextureId, frame_number: u64, bindless: &Bindless) {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return;
        };
        if slot.generation != id.generation {
            return;
        }
        if let Some(texture) = slot.texture.take() {
            if let Some(index) = texture.bindless {
                bindless.remove_sampled_image(index, frame_number);
                self.generation += 1;
            }
            self.live_bytes -= texture.bytes();
            self.retiring_bytes += texture.bytes();
            self.retired.push((frame_number, Retired::Texture(texture)));
        }
        slot.generation = slot.generation.wrapping_add(1);
        self.free_slots.push(id.index);
    }

    fn texture(&self, id: TextureId) -> Option<&PoolTexture> {
        let slot = self.slots.get(id.index as usize)?;
        (slot.generation == id.generation)
            .then_some(slot.texture.as_ref())
            .flatten()
    }

    fn texture_mut(&mut self, id: TextureId) -> Option<&mut PoolTexture> {
        let slot = self.slots.get_mut(id.index as usize)?;
        (slot.generation == id.generation)
            .then_some(slot.texture.as_mut())
            .flatten()
    }

    /// The bindless index of a texture once some of it is resident; `None` before, and
    /// for stale ids.
    pub fn index(&self, id: TextureId) -> Option<u32> {
        self.texture(id)?.bindless
    }

    /// A texture's image, for recording by hand (its uploaded levels are in
    /// `READ_ONLY_OPTIMAL` layout between frames; mip `i` is level `base + i`, see
    /// [`Self::info`]). It changes when the residency does ([`Self::set_base`]).
    pub fn image(&self, id: TextureId) -> Option<vk::Image> {
        Some(self.texture(id)?.image)
    }

    /// The largest resident level of a texture (0: all of it), once some of it is.
    pub fn resident_level(&self, id: TextureId) -> Option<u32> {
        let t = self.texture(id)?;
        (t.resident < t.levels).then_some(t.resident)
    }

    /// A texture's numbers; `None` for stale ids.
    pub fn info(&self, id: TextureId) -> Option<TextureInfo> {
        let t = self.texture(id)?;
        Some(TextureInfo {
            width: t.width,
            height: t.height,
            levels: t.levels,
            format: t.format,
            base: t.base,
            planned_base: t.planned_base(),
            resident: t.resident,
            uploaded: t.uploaded,
            bytes: t.bytes(),
        })
    }

    /// A counter that changes whenever textures become ready or are removed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Uploads waiting, and their bytes.
    pub fn pending(&self) -> (usize, u64) {
        (self.queue.len(), self.queue.iter().map(|u| u.bytes).sum())
    }

    /// Reallocations waiting ([`Self::set_base`]).
    pub fn pending_moves(&self) -> usize {
        self.moves.len()
    }

    /// Bytes uploaded so far.
    pub fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes
    }

    /// Bytes copied between images by reallocations so far.
    pub fn moved_bytes(&self) -> u64 {
        self.moved_bytes
    }

    /// Bytes of uploads and copies recorded by the last [`Self::begin_frame`].
    pub fn last_frame_bytes(&self) -> u64 {
        self.frame_bytes
    }

    /// GPU memory of the images: the live textures', and the retired ones' not freed yet
    /// (they are, once the frames that used them have finished).
    pub fn memory(&self) -> (u64, u64) {
        (self.live_bytes, self.retiring_bytes)
    }

    /// Frees what finished frames no longer use, makes pending reallocations and records
    /// queued uploads, oldest first, up to the budget (reallocations first: their copies
    /// count as their bytes). Uploaded levels are readable (and textures have bindless
    /// indices) in every pass declared after this call. Allocates only on frames that
    /// upload or reallocate.
    pub fn begin_frame(
        &mut self,
        fg: &mut FrameGraph<'_>,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame_number: u64,
    ) -> Result<(), RhiError> {
        engine_profiler::zone!("render::TexturePool::begin_frame");
        self.collect_retired(allocator, frame_number);
        self.frame_bytes = 0;
        if self.moves.is_empty() && self.queue.is_empty() {
            return Ok(());
        }
        let mut total = 0u64;
        // Reallocations: new images, the shared uploaded levels copied over.
        let mut moves: Vec<Move> = Vec::new();
        let mut changed: Vec<TextureId> = Vec::new();
        while let Some(&id) = self.moves.front() {
            let Some(t) = self.texture(id) else {
                self.moves.pop_front();
                continue;
            };
            let Some(base) = t.target else {
                self.moves.pop_front();
                continue;
            };
            let (width, height, format, levels, old_base, old_image) =
                (t.width, t.height, t.format, t.levels, t.base, t.image);
            let shared = t.uploaded & !((1u32 << base) - 1);
            let level_size = |k: u32| ((width >> k).max(1), (height >> k).max(1));
            let bytes: u64 = (0..levels)
                .filter(|k| shared & (1 << k) != 0)
                .map(|k| {
                    let (w, h) = level_size(k);
                    format.level_bytes(w, h)
                })
                .sum();
            if total > 0 && total + bytes > self.budget {
                break;
            }
            self.moves.pop_front();
            let (image, allocation) = create_image(allocator, width, height, format, levels, base)?;
            let mut m = Move {
                old: old_image,
                new: image,
                levels: [(0, 0, 0, 0); MAX_LEVELS as usize],
                count: 0,
            };
            for k in (0..levels).filter(|k| shared & (1 << k) != 0) {
                let (w, h) = level_size(k);
                m.levels[m.count as usize] = (k - old_base, k - base, w, h);
                m.count += 1;
            }
            total += bytes;
            self.moved_bytes += bytes;
            moves.push(m);
            let size = allocation.size();
            let Some(t) = self.texture_mut(id) else {
                unreachable!("looked up above");
            };
            let old_image = core::mem::replace(&mut t.image, image);
            let old_allocation = t.allocation.replace(allocation);
            let old_view = t.view.take();
            if let Some(index) = t.bindless.take() {
                bindless.remove_sampled_image(index, frame_number);
            }
            t.base = base;
            t.target = None;
            t.uploaded = shared;
            // Resident again once the view is made below (`levels` until then).
            t.resident = t.levels;
            changed.push(id);
            if let Some(old_allocation) = old_allocation {
                self.live_bytes = self.live_bytes - old_allocation.size() + size;
                self.retiring_bytes += old_allocation.size();
                self.retired.push((
                    frame_number,
                    Retired::Image(old_image, old_allocation, old_view),
                ));
            }
        }
        // This frame's uploads: the oldest within the budget.
        let mut batch = Vec::new();
        let mut owned_bytes = 0u64;
        while let Some(upload) = self.queue.front() {
            let holds = self
                .texture(upload.id)
                .is_some_and(|t| upload.first >= t.base);
            if !holds {
                // Removed before its upload, or its levels evicted since it was queued.
                if let Some(Upload {
                    source: Source::Span(span),
                    ..
                }) = self.queue.pop_front()
                {
                    self.cancel_staging(span);
                }
                continue;
            }
            if total > 0 && total + upload.bytes > self.budget {
                break;
            }
            total += upload.bytes;
            if matches!(upload.source, Source::Owned(_)) {
                owned_bytes += upload.bytes;
            }
            if let Some(upload) = self.queue.pop_front() {
                batch.push(upload);
            }
        }
        self.frame_bytes = total;
        if batch.is_empty() && moves.is_empty() {
            return Ok(());
        }
        // In-memory levels go through a staging buffer of this frame's.
        let owned_staging = if owned_bytes > 0 {
            let (buffer, mut memory) = allocator.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(owned_bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                MemoryLocation::Upload,
            )?;
            let Some(target) = memory.mapped_slice_mut() else {
                // SAFETY: never used.
                unsafe { allocator.destroy_buffer(buffer, memory) };
                return Err(RhiError::Vulkan {
                    call: "staging buffer not mapped",
                    result: vk::Result::ERROR_MEMORY_MAP_FAILED,
                });
            };
            let mut offset = 0usize;
            for upload in &batch {
                if let Source::Owned(levels) = &upload.source {
                    for level in levels {
                        target[offset..offset + level.len()].copy_from_slice(level);
                        offset += (level.len() as u64).next_multiple_of(LEVEL_ALIGN) as usize;
                    }
                }
            }
            Some((buffer, memory))
        } else {
            None
        };
        let owned_handle = owned_staging.as_ref().map(|(buffer, _)| {
            fg.import_buffer(
                "texture staging",
                ImportedBuffer {
                    buffer: *buffer,
                    size: owned_bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: None,
                },
            )
        });
        let ring_handle = self.ring.as_ref().and_then(|ring| {
            batch
                .iter()
                .any(|u| matches!(u.source, Source::Span(_)))
                .then(|| {
                    fg.import_buffer(
                        "texture streaming ring",
                        ImportedBuffer {
                            buffer: ring.buffer,
                            size: ring.spans.size + STAGING_ALIGN,
                            address: 0,
                            initial: BufferState::idle(),
                            final_state: None,
                        },
                    )
                })
        });
        // Copies per upload: (image, first mip, count, from the ring?, regions).
        let mut copies: Vec<(vk::Image, u32, u32, bool, Vec<vk::BufferImageCopy>)> =
            Vec::with_capacity(batch.len());
        let mut owned_offset = 0u64;
        for upload in &batch {
            let Some(t) = self.texture(upload.id) else {
                continue;
            };
            let (mut offset, ring) = match &upload.source {
                Source::Owned(_) => (owned_offset, false),
                Source::Span(span) => (span.offset, true),
            };
            let mut regions = Vec::with_capacity(upload.count as usize);
            for k in upload.first..upload.first + upload.count {
                let (w, h) = t.level_size(k);
                regions.push(vk::BufferImageCopy {
                    buffer_offset: offset,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: k - t.base,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: w,
                        height: h,
                        depth: 1,
                    },
                    ..Default::default()
                });
                offset += t.format.level_bytes(w, h).next_multiple_of(LEVEL_ALIGN);
            }
            if !ring {
                owned_offset = offset;
            }
            copies.push((t.image, upload.first - t.base, upload.count, ring, regions));
        }
        let mut pass = fg.add_pass("texture pool upload");
        if let Some(handle) = owned_handle {
            pass = pass.read(handle, Access::TransferRead);
        }
        if let Some(handle) = ring_handle {
            pass = pass.read(handle, Access::TransferRead);
        }
        pass.side_effects().execute(move |ctx| {
            let range = |first, count| vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: first,
                level_count: count,
                base_array_layer: 0,
                layer_count: 1,
            };
            let transition = |image, first, count, from: Layout, to: Layout| {
                vk::ImageMemoryBarrier2::default()
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(range(first, count))
                    .src_stage_mask(from.stages)
                    .src_access_mask(from.access)
                    .old_layout(from.layout)
                    .dst_stage_mask(to.stages)
                    .dst_access_mask(to.access)
                    .new_layout(to.layout)
            };
            // Reallocations: the old images' copied levels become copy sources (after the
            // earlier frames' reads), the new images copy destinations; the copied levels
            // are readable after.
            let mut to_copy = Vec::with_capacity(moves.len() * 2);
            let mut copied = Vec::with_capacity(moves.len() * 2);
            for m in &moves {
                for &(old, new, _, _) in &m.levels[..m.count as usize] {
                    to_copy.push(transition(m.old, old, 1, READ, COPY_SOURCE));
                    to_copy.push(transition(m.new, new, 1, UNDEFINED, COPY_TARGET));
                    copied.push(transition(m.new, new, 1, COPY_TARGET, READ));
                }
            }
            let to_transfer: Vec<_> = copies
                .iter()
                .map(|(image, first, count, _, _)| {
                    transition(*image, *first, *count, UNDEFINED, COPY_TARGET)
                })
                .collect();
            let to_read: Vec<_> = copies
                .iter()
                .map(|(image, first, count, _, _)| {
                    transition(*image, *first, *count, COPY_TARGET, READ)
                })
                .collect();
            let owned = owned_handle.map(|h| ctx.buffer(h));
            let ring = ring_handle.map(|h| ctx.buffer(h));
            // SAFETY: recording outside a rendering scope. Uploaded levels are new (never
            // used before these barriers: a level is uploaded once per image); copied
            // levels were uploaded in earlier frames (read-only since) into images the pool
            // owns, which it frees only after the frames that used them.
            unsafe {
                if !to_copy.is_empty() {
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&to_copy),
                    );
                    for m in &moves {
                        let regions: Vec<vk::ImageCopy> = m.levels[..m.count as usize]
                            .iter()
                            .map(|&(old, new, w, h)| vk::ImageCopy {
                                src_subresource: vk::ImageSubresourceLayers {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    mip_level: old,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                },
                                dst_subresource: vk::ImageSubresourceLayers {
                                    aspect_mask: vk::ImageAspectFlags::COLOR,
                                    mip_level: new,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                },
                                extent: vk::Extent3D {
                                    width: w,
                                    height: h,
                                    depth: 1,
                                },
                                ..Default::default()
                            })
                            .collect();
                        ctx.device.cmd_copy_image(
                            ctx.cmd,
                            m.old,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            m.new,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            &regions,
                        );
                    }
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&copied),
                    );
                }
                if !copies.is_empty() {
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&to_transfer),
                    );
                    for (image, _, _, from_ring, regions) in &copies {
                        let source = if *from_ring { ring } else { owned };
                        if let Some(source) = source {
                            ctx.device.cmd_copy_buffer_to_image(
                                ctx.cmd,
                                source,
                                *image,
                                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                                regions,
                            );
                        }
                    }
                    ctx.device.cmd_pipeline_barrier2(
                        ctx.cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&to_read),
                    );
                }
            }
        });
        // Residency: views over the resident levels, a new bindless index when it grows.
        for upload in batch {
            let mask = ((1u64 << (upload.first + upload.count)) - (1u64 << upload.first)) as u32;
            if let Source::Span(span) = upload.source {
                self.retired.push((frame_number, Retired::Span(span.start)));
            }
            self.uploaded_bytes += upload.bytes;
            if let Some(t) = self.texture_mut(upload.id) {
                t.uploaded |= mask;
                changed.push(upload.id);
            }
        }
        for id in changed {
            self.refresh_view(id, bindless, frame_number)?;
        }
        self.generation += 1;
        if let Some(staging) = owned_staging {
            self.retired
                .push((frame_number, Retired::Staging(staging.0, staging.1)));
        }
        Ok(())
    }

    /// Gives a texture a view over its resident levels (the uploaded run from its smallest
    /// level up) and a new bindless index when they changed.
    fn refresh_view(
        &mut self,
        id: TextureId,
        bindless: &Bindless,
        frame_number: u64,
    ) -> Result<(), RhiError> {
        let device = self.device.clone();
        let Some(t) = self.texture_mut(id) else {
            return Ok(());
        };
        let mut resident = t.levels;
        while resident > t.base && t.uploaded & (1 << (resident - 1)) != 0 {
            resident -= 1;
        }
        if resident == t.resident && t.view.is_some() {
            return Ok(());
        }
        let mut retired_view = None;
        if resident < t.levels {
            // SAFETY: a view of the pool's image over levels it holds and has uploaded.
            let view = unsafe {
                device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(t.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(t.format.vk())
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: resident - t.base,
                            level_count: t.levels - resident,
                            base_array_layer: 0,
                            layer_count: 1,
                        }),
                    None,
                )
            };
            let view = view.map_err(RhiError::vk("vkCreateImageView"))?;
            if let Some(old) = t.bindless.take() {
                bindless.remove_sampled_image(old, frame_number);
            }
            retired_view = t.view.replace(view);
            t.bindless = bindless.add_sampled_image(view);
        }
        t.resident = resident;
        if let Some(old) = retired_view {
            self.retired.push((frame_number, Retired::View(old)));
        }
        Ok(())
    }

    fn collect_retired(&mut self, allocator: &mut Allocator, frame_number: u64) {
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (_, retired) = self.retired.swap_remove(i);
            // SAFETY: the frames that used it have finished.
            unsafe { self.destroy_retired(allocator, retired) };
        }
    }

    /// # Safety
    ///
    /// The GPU no longer uses it.
    unsafe fn destroy_retired(&mut self, allocator: &mut Allocator, retired: Retired) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            match retired {
                Retired::Texture(mut texture) => {
                    if let Some(view) = texture.view.take() {
                        self.device.destroy_image_view(view, None);
                    }
                    if let Some(allocation) = texture.allocation.take() {
                        self.retiring_bytes -= allocation.size();
                        allocator.destroy_image(texture.image, allocation);
                    }
                }
                Retired::Image(image, allocation, view) => {
                    if let Some(view) = view {
                        self.device.destroy_image_view(view, None);
                    }
                    self.retiring_bytes -= allocation.size();
                    allocator.destroy_image(image, allocation);
                }
                Retired::View(view) => self.device.destroy_image_view(view, None),
                Retired::Staging(buffer, allocation) => {
                    allocator.destroy_buffer(buffer, allocation);
                }
                Retired::Span(start) => {
                    if let Some(ring) = self.ring.as_mut() {
                        ring.give_back(start);
                    }
                }
            }
        }
    }

    /// Frees every texture and the staging ring.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses the pool (the device is idle), and no staging span is being
    /// filled.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        let slots = core::mem::take(&mut self.slots);
        for slot in slots {
            if let Some(texture) = slot.texture {
                if let Some(index) = texture.bindless {
                    bindless.remove_sampled_image(index, 0);
                }
                self.live_bytes -= texture.bytes();
                self.retiring_bytes += texture.bytes();
                // SAFETY: guaranteed by the caller.
                unsafe { self.destroy_retired(allocator, Retired::Texture(texture)) };
            }
        }
        for (_, retired) in core::mem::take(&mut self.retired) {
            // SAFETY: guaranteed by the caller.
            unsafe { self.destroy_retired(allocator, retired) };
        }
        self.queue.clear();
        self.moves.clear();
        if let Some(ring) = self.ring.take() {
            // SAFETY: guaranteed by the caller.
            unsafe { allocator.destroy_buffer(ring.buffer, ring.allocation) };
        }
    }
}

/// A layout with the stages and accesses that use it, for the pool's own barriers.
#[derive(Clone, Copy)]
struct Layout {
    stages: vk::PipelineStageFlags2,
    access: vk::AccessFlags2,
    layout: vk::ImageLayout,
}

const UNDEFINED: Layout = Layout {
    stages: vk::PipelineStageFlags2::NONE,
    access: vk::AccessFlags2::NONE,
    layout: vk::ImageLayout::UNDEFINED,
};
/// Sampled by any shader of the frames (the whole queue's earlier work).
const READ: Layout = Layout {
    stages: vk::PipelineStageFlags2::ALL_COMMANDS,
    access: vk::AccessFlags2::SHADER_SAMPLED_READ,
    layout: vk::ImageLayout::READ_ONLY_OPTIMAL,
};
const COPY_SOURCE: Layout = Layout {
    stages: vk::PipelineStageFlags2::COPY,
    access: vk::AccessFlags2::TRANSFER_READ,
    layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
};
const COPY_TARGET: Layout = Layout {
    stages: vk::PipelineStageFlags2::COPY,
    access: vk::AccessFlags2::TRANSFER_WRITE,
    layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
};

/// An image holding levels `base..levels` of a texture.
fn create_image(
    allocator: &mut Allocator,
    width: u32,
    height: u32,
    format: TextureFormat,
    levels: u32,
    base: u32,
) -> Result<(vk::Image, Allocation), RhiError> {
    allocator.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format.vk())
            .extent(vk::Extent3D {
                width: (width >> base).max(1),
                height: (height >> base).max(1),
                depth: 1,
            })
            .mip_levels(levels - base)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            // Copied from too: into an image of more or fewer levels when the residency
            // changes, and by tools reading textures back.
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .initial_layout(vk::ImageLayout::UNDEFINED),
        MemoryLocation::Device,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_lends_in_order_and_takes_back_in_any() {
        const B: u64 = STAGING_ALIGN;
        let mut r = RingSpans {
            size: 16 * B,
            ..RingSpans::default()
        };
        let a = r.lend(5 * B).unwrap();
        let b = r.lend(1).unwrap();
        assert_eq!((a.1, a.2), (0, 5 * B));
        assert_eq!((b.1, b.2), (5 * B, B));
        let c = r.lend(8 * B).unwrap();
        // 14 blocks out, 2 left at the end: 3 do not fit.
        assert!(r.lend(3 * B).is_none());
        // `b` back first: `a` still holds the front.
        r.give_back(b.0);
        assert!(r.lend(3 * B).is_none());
        r.give_back(a.0);
        // 6 blocks free at the front: 3 skip the 2 at the end and start at 0.
        let d = r.lend(3 * B).unwrap();
        assert_eq!(d.1, 0);
        r.give_back(c.0);
        r.give_back(d.0);
        assert!(r.spans.is_empty());
        // Empty again: the whole ring at once, from wherever the head is.
        let e = r.lend(16 * B).unwrap();
        assert_eq!((e.1, e.2), (0, 16 * B));
        assert!(r.lend(1).is_none());
    }
}
