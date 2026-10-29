//! Vulkan objects behind graph resources.
//!
//! Transient resources are grouped by kind and allowed memory types, placed with aliasing
//! ([`super::placement`]) and bound at their offsets.
//!
//! - **Heaps the frame slots share.** A group whose resources only the graphics queue
//!   uses lives in a heap every frame slot (frame in flight) shares. Frames run on that
//!   queue in order, and each frame's first graphics batch begins with a barrier on
//!   everything before it ([`Realized::shared_heap_bytes`]). A shared heap replaced by a
//!   larger one is freed once every slot is past the frames that used it.
//! - **Heaps of each slot.** A group the async compute queue touches (ADR 0065) runs
//!   beside the graphics queue, also beside the previous frame's graphics work: each slot
//!   owns a heap for it. The group is not split by queue: its resources keep aliasing one
//!   another (split, the RTX's frame needed 135 MiB for its two slots instead of 130).
//! - **Objects.** The images, buffers and views are cached per slot by (description,
//!   usage, heap, offset): while the frame's layout repeats, nothing is created. A slot
//!   keeps the placements of its last few frame shapes ([`PLACEMENTS_PER_GROUP`]) and
//!   objects no frame used for up to [`KEEP_UNUSED`] of its frames: a shape that comes
//!   back (a frame that captures a reflection probe every 30 frames, between frames that
//!   do not) finds its objects instead of making every transient again, twice. Objects
//!   unused for longer are destroyed when their slot comes around, the GPU done with them.

use super::compile::{Kind, QueueKind, Records};
use super::placement::{self, Item};
use super::{BufferDesc, TextureDesc, aspect_of};
use engine_rhi::ash::{self, vk};
use engine_rhi::{
    Allocation, AllocationRequest, Allocator, Bindless, MemoryLocation, ResourceKind, RhiError,
};

/// The least alignment of a transient buffer (see `requirements_of`).
const TRANSIENT_BUFFER_ALIGN: u64 = 256;

/// Frames of its slot an object no frame used is kept for.
pub(crate) const KEEP_UNUSED: u64 = 128;

/// Placements kept per group and slot: the frame shapes that alternate.
pub(crate) const PLACEMENTS_PER_GROUP: usize = 4;

/// Layers of a layered texture that get a view of their own each: what a pass renders
/// into one by one ([`super::PassBuilder::depth_layers`]).
pub(crate) const MAX_LAYER_VIEWS: usize = 6;

/// The Vulkan side of one graph resource in the current frame.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Physical {
    pub(crate) image: vk::Image,
    pub(crate) view: vk::ImageView,
    /// A view of each of its first layers, for an array texture that is an attachment
    /// (null otherwise).
    pub(crate) layer_views: [vk::ImageView; MAX_LAYER_VIEWS],
    pub(crate) buffer: vk::Buffer,
    pub(crate) address: vk::DeviceAddress,
    pub(crate) extent: vk::Extent2D,
    /// Index in the bindless sampled-image binding, if registered.
    pub(crate) sampled_index: Option<u32>,
    /// Index in the bindless storage-image binding, if registered.
    pub(crate) storage_index: Option<u32>,
}

/// Results of [`PhysicalResources::realize`] for the stats.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Realized {
    pub(crate) transient_resources: u32,
    /// Bytes of every heap the frame's transients use: its slot's and the shared ones.
    pub(crate) heap_bytes: u64,
    /// Of those, the shared heaps' bytes: when not zero, the frame's first graphics batch
    /// waits for everything before it on that queue.
    pub(crate) shared_heap_bytes: u64,
    pub(crate) placed_bytes: u64,
    pub(crate) unaliased_bytes: u64,
    pub(crate) created_objects: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GroupKey {
    kind: Kind,
    type_bits: u32,
    /// Only the graphics queue uses its resources: placed in a heap the slots share.
    shared: bool,
}

struct Heap {
    key: GroupKey,
    allocation: Allocation,
    size: u64,
    id: u64,
}

struct CachedImage {
    desc: TextureDesc,
    usage: vk::ImageUsageFlags,
    concurrent: bool,
    heap: u64,
    offset: u64,
    image: vk::Image,
    view: vk::ImageView,
    layer_views: [vk::ImageView; MAX_LAYER_VIEWS],
    sampled_index: Option<u32>,
    storage_index: Option<u32>,
    used: bool,
    /// The slot's frame that last used it.
    last_used: u64,
}

impl CachedImage {
    /// Registers the view in the bindless set for the usages that need it (once).
    fn register(&mut self, bindless: Option<&Bindless>) {
        let Some(bindless) = bindless else { return };
        if self.view == vk::ImageView::null() {
            return;
        }
        if self.sampled_index.is_none() && self.usage.contains(vk::ImageUsageFlags::SAMPLED) {
            self.sampled_index = bindless.add_sampled_image(self.view);
        }
        if self.storage_index.is_none() && self.usage.contains(vk::ImageUsageFlags::STORAGE) {
            self.storage_index = bindless.add_storage_image(self.view);
        }
    }

    /// Destroys the image and view and returns its bindless indices.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses the image (its indices are free to reuse at once).
    unsafe fn destroy(&self, device: &ash::Device, bindless: Option<&Bindless>) {
        if let Some(bindless) = bindless {
            if let Some(index) = self.sampled_index {
                bindless.remove_sampled_image(index, 0);
            }
            if let Some(index) = self.storage_index {
                bindless.remove_storage_image(index, 0);
            }
        }
        // SAFETY: guaranteed by the caller.
        unsafe {
            for view in self.layer_views {
                if view != vk::ImageView::null() {
                    device.destroy_image_view(view, None);
                }
            }
            if self.view != vk::ImageView::null() {
                device.destroy_image_view(self.view, None);
            }
            device.destroy_image(self.image, None);
        }
    }
}

struct CachedBuffer {
    size: u64,
    usage: vk::BufferUsageFlags,
    concurrent: bool,
    heap: u64,
    offset: u64,
    buffer: vk::Buffer,
    address: vk::DeviceAddress,
    used: bool,
    /// The slot's frame that last used it.
    last_used: u64,
}

/// The placement of one group in a recent frame of a slot, reused when the frame's shape
/// (sizes, lifetimes, resource order) is that frame's again.
struct CachedPlacement {
    key: GroupKey,
    hash: u128,
    heap_size: u64,
    offsets: Vec<u64>,
    alias_ranges: Vec<(u32, u32)>,
    alias_resources: Vec<u32>,
    /// The slot's frame that last used it.
    last_used: u64,
}

#[derive(Default)]
struct Slot {
    /// Frames realized in this slot.
    frames: u64,
    heaps: Vec<Heap>,
    images: Vec<CachedImage>,
    buffers: Vec<CachedBuffer>,
    placements: Vec<CachedPlacement>,
    /// Where the next cache lookup starts: frames repeat their order, so lookups hit at
    /// the cursor in steady state.
    image_cursor: usize,
    buffer_cursor: usize,
}

/// Index of the first element matching `matches`, searching from `cursor` and wrapping.
fn find_from<T>(list: &[T], cursor: usize, mut matches: impl FnMut(&T) -> bool) -> Option<usize> {
    let cursor = cursor.min(list.len());
    (cursor..list.len())
        .chain(0..cursor)
        .find(|&i| matches(&list[i]))
}

pub(crate) struct PhysicalResources {
    slots: Vec<Slot>,
    current: Vec<Physical>,
    image_requirements: Vec<(
        TextureDesc,
        vk::ImageUsageFlags,
        bool,
        vk::MemoryRequirements,
    )>,
    buffer_requirements: Vec<(u64, vk::BufferUsageFlags, bool, vk::MemoryRequirements)>,
    /// The graphics and async compute queue families, for resources shared concurrently.
    families: [u32; 2],
    requirements: Vec<vk::MemoryRequirements>,
    groups: Vec<GroupKey>,
    items: Vec<Item>,
    item_resources: Vec<u32>,
    scratch: placement::Scratch,
    predecessor_scratch: placement::Scratch,
    predecessors: Vec<u32>,
    next_heap_id: u64,
    /// Heaps of the resources only the graphics queue uses, shared by the slots.
    shared: Vec<Heap>,
    /// Shared heaps replaced by larger ones, and the realization that replaced them: freed
    /// once every slot has been realized since.
    retired: Vec<(Heap, u64)>,
    /// Realizations so far.
    realizations: u64,
    /// By resource: a pass on the async compute queue uses it (scratch).
    async_used: Vec<bool>,
}

/// Create info of a transient image; `families` for concurrent sharing between queues.
fn image_info<'a>(
    desc: &TextureDesc,
    usage: vk::ImageUsageFlags,
    families: Option<&'a [u32; 2]>,
) -> vk::ImageCreateInfo<'a> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(desc.format)
        .extent(vk::Extent3D {
            width: desc.width.max(1),
            height: desc.height.max(1),
            depth: 1,
        })
        .mip_levels(desc.mip_levels.max(1))
        .array_layers(desc.layers.max(1))
        .samples(desc.samples)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    match families {
        Some(families) => info
            .sharing_mode(vk::SharingMode::CONCURRENT)
            .queue_family_indices(families),
        None => info.sharing_mode(vk::SharingMode::EXCLUSIVE),
    }
}

/// Create info of a transient buffer; `families` for concurrent sharing between queues.
fn buffer_info<'a>(
    size: u64,
    usage: vk::BufferUsageFlags,
    families: Option<&'a [u32; 2]>,
) -> vk::BufferCreateInfo<'a> {
    let info = vk::BufferCreateInfo::default()
        .size(size.max(4))
        .usage(usage | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS);
    match families {
        Some(families) => info
            .sharing_mode(vk::SharingMode::CONCURRENT)
            .queue_family_indices(families),
        None => info.sharing_mode(vk::SharingMode::EXCLUSIVE),
    }
}

impl PhysicalResources {
    /// `families`: the graphics and async compute queue families (for transients shared
    /// concurrently between them).
    pub(crate) fn new(slots: usize, families: [u32; 2]) -> Self {
        Self {
            families,
            slots: (0..slots).map(|_| Slot::default()).collect(),
            current: Vec::new(),
            image_requirements: Vec::new(),
            buffer_requirements: Vec::new(),
            requirements: Vec::new(),
            groups: Vec::new(),
            items: Vec::new(),
            item_resources: Vec::new(),
            scratch: placement::Scratch::default(),
            predecessor_scratch: placement::Scratch::default(),
            predecessors: Vec::new(),
            next_heap_id: 1,
            shared: Vec::new(),
            retired: Vec::new(),
            realizations: 0,
            async_used: Vec::new(),
        }
    }

    pub(crate) fn slots(&self) -> usize {
        self.slots.len()
    }

    /// Physical objects of the current frame's resources, by resource id.
    pub(crate) fn current(&self) -> &[Physical] {
        &self.current
    }

    #[allow(clippy::too_many_arguments)]
    fn requirements_of(
        &mut self,
        device: &ash::Device,
        kind: Kind,
        texture: &TextureDesc,
        buffer: &BufferDesc,
        image_usage: vk::ImageUsageFlags,
        buffer_usage: vk::BufferUsageFlags,
        concurrent: bool,
    ) -> vk::MemoryRequirements {
        let families = self.families;
        let families = concurrent.then_some(&families);
        match kind {
            Kind::Texture => {
                if let Some(&(_, _, _, r)) = self
                    .image_requirements
                    .iter()
                    .find(|(d, u, c, _)| d == texture && *u == image_usage && *c == concurrent)
                {
                    return r;
                }
                let info = image_info(texture, image_usage, families);
                let mut out = vk::MemoryRequirements2::default();
                // SAFETY: valid create info (maintenance4 is core in Vulkan 1.3).
                unsafe {
                    device.get_device_image_memory_requirements(
                        &vk::DeviceImageMemoryRequirements::default().create_info(&info),
                        &mut out,
                    );
                }
                let r = out.memory_requirements;
                self.image_requirements
                    .push((*texture, image_usage, concurrent, r));
                r
            }
            Kind::Buffer => {
                if let Some(&(_, _, _, r)) = self.buffer_requirements.iter().find(|(s, u, c, _)| {
                    *s == buffer.size && *u == buffer_usage && *c == concurrent
                }) {
                    return r;
                }
                let info = buffer_info(buffer.size, buffer_usage, families);
                let mut out = vk::MemoryRequirements2::default();
                // SAFETY: valid create info.
                unsafe {
                    device.get_device_buffer_memory_requirements(
                        &vk::DeviceBufferMemoryRequirements::default().create_info(&info),
                        &mut out,
                    );
                }
                // Shaders read buffers by address with 16-byte loads (ADR 0013's
                // addendum) and acceleration structure builds take 16-byte-aligned
                // instances (ADR 0087): drivers may ask for less (4 on AMD's).
                let mut r = out.memory_requirements;
                r.alignment = r.alignment.max(TRANSIENT_BUFFER_ALIGN);
                self.buffer_requirements
                    .push((buffer.size, buffer_usage, concurrent, r));
                r
            }
        }
    }

    /// Places, allocates and binds this frame's transient resources, fills
    /// [`Self::current`], and records which resources alias which (for the barriers).
    ///
    /// # Safety
    ///
    /// The GPU has finished every submission that used `slot`'s transient resources, and
    /// the frame's first graphics batch waits for everything before it on that queue when
    /// [`Realized::shared_heap_bytes`] is not zero (the shared heaps').
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn realize(
        &mut self,
        slot: usize,
        device: &ash::Device,
        debug_utils: Option<&ash::ext::debug_utils::Device>,
        bindless: Option<&Bindless>,
        allocator: &mut Allocator,
        records: &mut Records,
        textures: &[TextureDesc],
        buffers: &[BufferDesc],
        imports: &[Physical],
    ) -> Result<Realized, RhiError> {
        let mut realized = Realized::default();
        self.current.clear();
        self.current.extend_from_slice(imports);
        self.slots[slot].frames += 1;
        let frame = self.slots[slot].frames;

        // Shared heaps replaced at least a round of slots ago: no frame in flight uses them
        // (each slot's previous frame is done by the time it is realized).
        self.realizations += 1;
        let slot_count = self.slots.len() as u64;
        while let Some(i) = self
            .retired
            .iter()
            .position(|(_, at)| self.realizations >= at + slot_count)
        {
            let (heap, _) = self.retired.swap_remove(i);
            for slot in &mut self.slots {
                // SAFETY: no pending GPU work uses them (above).
                unsafe { destroy_heap_objects(device, bindless, slot, heap.id) };
            }
            allocator.free(heap.allocation);
        }

        // What the async compute queue touches: it runs beside the graphics queue, so its
        // transients keep heaps of their slot.
        self.async_used.clear();
        self.async_used.resize(records.resources.len(), false);
        for pass in records
            .passes
            .iter()
            .filter(|p| p.needed && p.queue == QueueKind::AsyncCompute)
        {
            for a in &records.accesses[pass.accesses.start as usize..pass.accesses.end as usize] {
                self.async_used[a.resource as usize] = true;
            }
        }

        // Requirements and groups of the used transient resources.
        self.requirements.clear();
        self.groups.clear();
        for r in 0..records.resources.len() {
            let resource = &records.resources[r];
            if !resource.is_transient() || !resource.is_used() {
                self.requirements.push(vk::MemoryRequirements::default());
                continue;
            }
            let (kind, image_usage, buffer_usage, concurrent) = (
                resource.kind,
                resource.image_usage,
                resource.buffer_usage,
                resource.concurrent,
            );
            let req = self.requirements_of(
                device,
                kind,
                &textures[r],
                &buffers[r],
                image_usage,
                buffer_usage,
                concurrent,
            );
            self.requirements.push(req);
            let key = GroupKey {
                kind,
                type_bits: req.memory_type_bits,
                shared: true,
            };
            match self
                .groups
                .iter_mut()
                .find(|g| g.kind == key.kind && g.type_bits == key.type_bits)
            {
                // A group the async compute queue touches keeps heaps per slot.
                Some(g) => g.shared &= !self.async_used[r],
                None => self.groups.push(GroupKey {
                    shared: !self.async_used[r],
                    ..key
                }),
            }
            realized.transient_resources += 1;
            realized.unaliased_bytes += req.size;
        }

        let slot_index = slot;
        for g in 0..self.groups.len() {
            let key = self.groups[g];
            self.items.clear();
            self.item_resources.clear();
            let mut align = 1;
            for (r, resource) in records.resources.iter().enumerate() {
                let req = self.requirements[r];
                if !resource.is_transient()
                    || !resource.is_used()
                    || resource.kind != key.kind
                    || req.memory_type_bits != key.type_bits
                {
                    continue;
                }
                align = align.max(req.alignment);
                self.items.push(Item {
                    size: req.size,
                    align: req.alignment,
                    first: resource.first,
                    last: resource.last,
                    offset: 0,
                });
                self.item_resources.push(r as u32);
            }
            // Placement and aliasing: reused while the frame's shape repeats.
            let mut hasher = engine_core::hash::StableHasher::new();
            for (item, &r) in self.items.iter().zip(&self.item_resources) {
                hasher.write_u64(item.size);
                hasher.write_u64(item.align);
                hasher.write_u64(u64::from(item.first) << 32 | u64::from(item.last));
                hasher.write_u64(u64::from(r));
            }
            for &r in &self.item_resources {
                hasher.write_u64(u64::from(records.resources[r as usize].concurrent));
            }
            let hash = hasher.finish();
            let slot = &mut self.slots[slot_index];
            // This shape's placement, else a new one: in a new entry while the group has
            // fewer than `PLACEMENTS_PER_GROUP`, else in its least recently used.
            let found = slot
                .placements
                .iter()
                .position(|p| p.key == key && p.hash == hash);
            let cache_index = match found {
                Some(i) => i,
                None if slot.placements.iter().filter(|p| p.key == key).count()
                    < PLACEMENTS_PER_GROUP =>
                {
                    slot.placements.push(CachedPlacement {
                        key,
                        hash: !hash,
                        heap_size: 0,
                        offsets: Vec::new(),
                        alias_ranges: Vec::new(),
                        alias_resources: Vec::new(),
                        last_used: frame,
                    });
                    slot.placements.len() - 1
                }
                None => slot
                    .placements
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.key == key)
                    .min_by_key(|(_, p)| p.last_used)
                    .map_or(0, |(i, _)| i),
            };
            let cache = &mut slot.placements[cache_index];
            cache.last_used = frame;
            let room = (
                cache.offsets.capacity(),
                cache.alias_ranges.capacity(),
                cache.alias_resources.capacity(),
            );
            if cache.hash != hash {
                cache.heap_size = placement::place(&mut self.items, &mut self.scratch);
                cache.offsets.clear();
                cache.offsets.extend(self.items.iter().map(|i| i.offset));
                cache.alias_ranges.clear();
                cache.alias_resources.clear();
                for i in 0..self.items.len() {
                    self.predecessors.clear();
                    placement::predecessors(
                        &self.items,
                        i,
                        &mut self.predecessors,
                        &mut self.predecessor_scratch,
                    );
                    let start = cache.alias_resources.len() as u32;
                    cache.alias_resources.extend(
                        self.predecessors
                            .iter()
                            .map(|&j| self.item_resources[j as usize]),
                    );
                    cache
                        .alias_ranges
                        .push((start, cache.alias_resources.len() as u32));
                }
                cache.hash = hash;
            } else {
                for (item, &offset) in self.items.iter_mut().zip(&cache.offsets) {
                    item.offset = offset;
                }
            }
            let heap_size = cache.heap_size;
            realized.placed_bytes += heap_size;
            let base_alias = records.aliases.len() as u32;
            records.aliases.extend_from_slice(&cache.alias_resources);
            for (&(start, end), &r) in cache.alias_ranges.iter().zip(&self.item_resources) {
                records.resources[r as usize].aliases = base_alias + start..base_alias + end;
            }
            // A placement that grew gives every one kept the same room: a shape placed again
            // into any of them (one coming back after the least recent was replaced, or seen
            // in the other slot) allocates nothing.
            let grown = (
                cache.offsets.capacity(),
                cache.alias_ranges.capacity(),
                cache.alias_resources.capacity(),
            );
            if grown != room {
                for placement in self.slots.iter_mut().flat_map(|s| &mut s.placements) {
                    let len = placement.offsets.len();
                    placement.offsets.reserve(grown.0.saturating_sub(len));
                    let len = placement.alias_ranges.len();
                    placement.alias_ranges.reserve(grown.1.saturating_sub(len));
                    let len = placement.alias_resources.len();
                    placement.alias_resources.reserve(grown.2.saturating_sub(len));
                }
            }

            // The heap: reuse, or replace when too small.
            let heap = if key.shared {
                let existing = self.shared.iter().position(|h| h.key == key);
                let index = match existing {
                    Some(i) if self.shared[i].size >= heap_size => i,
                    _ => {
                        if let Some(i) = existing {
                            // The other slots' frames in flight may still use it.
                            let old = self.shared.swap_remove(i);
                            self.retired.push((old, self.realizations));
                        }
                        let heap = allocate_heap(
                            allocator,
                            key,
                            heap_size,
                            align,
                            &mut self.next_heap_id,
                        )?;
                        self.shared.push(heap);
                        self.shared.len() - 1
                    }
                };
                realized.shared_heap_bytes += self.shared[index].size;
                &self.shared[index]
            } else {
                let slot = &mut self.slots[slot_index];
                let existing = slot.heaps.iter().position(|h| h.key == key);
                let index = match existing {
                    Some(i) if slot.heaps[i].size >= heap_size => i,
                    _ => {
                        if let Some(i) = existing {
                            let old = slot.heaps.swap_remove(i);
                            // SAFETY: the slot's previous work is done (caller).
                            unsafe { destroy_heap_objects(device, bindless, slot, old.id) };
                            allocator.free(old.allocation);
                        }
                        let heap = allocate_heap(
                            allocator,
                            key,
                            heap_size,
                            align,
                            &mut self.next_heap_id,
                        )?;
                        slot.heaps.push(heap);
                        slot.heaps.len() - 1
                    }
                };
                &self.slots[slot_index].heaps[index]
            };
            let (heap_id, memory, base) =
                (heap.id, heap.allocation.memory(), heap.allocation.offset());
            realized.heap_bytes += heap.size;
            let slot = &mut self.slots[slot_index];

            for (item, &r) in self.items.iter().zip(&self.item_resources) {
                let r = r as usize;
                let resource = &records.resources[r];
                match key.kind {
                    Kind::Texture => {
                        let desc = &textures[r];
                        let usage = resource.image_usage;
                        let concurrent = resource.concurrent;
                        let cached = find_from(&slot.images, slot.image_cursor, |c| {
                            !c.used
                                && c.heap == heap_id
                                && c.offset == item.offset
                                && c.desc == *desc
                                && c.usage == usage
                                && c.concurrent == concurrent
                        });
                        let index = match cached {
                            Some(i) => {
                                slot.image_cursor = i + 1;
                                slot.images[i].used = true;
                                slot.images[i].last_used = frame;
                                i
                            }
                            None => {
                                let families = concurrent.then_some(&self.families);
                                // SAFETY: memory and offset come from the heap and placement.
                                let (image, view, layer_views) = unsafe {
                                    create_image(
                                        device,
                                        desc,
                                        usage,
                                        families,
                                        memory,
                                        base + item.offset,
                                    )
                                }?;
                                name_object(debug_utils, image, resource.name);
                                slot.images.push(CachedImage {
                                    desc: *desc,
                                    usage,
                                    concurrent,
                                    heap: heap_id,
                                    offset: item.offset,
                                    image,
                                    view,
                                    layer_views,
                                    sampled_index: None,
                                    storage_index: None,
                                    used: true,
                                    last_used: frame,
                                });
                                realized.created_objects += 1;
                                slot.images.len() - 1
                            }
                        };
                        let c = &mut slot.images[index];
                        c.register(bindless);
                        self.current[r] = Physical {
                            image: c.image,
                            view: c.view,
                            layer_views: c.layer_views,
                            extent: vk::Extent2D {
                                width: desc.width,
                                height: desc.height,
                            },
                            sampled_index: c.sampled_index,
                            storage_index: c.storage_index,
                            ..Physical::default()
                        };
                    }
                    Kind::Buffer => {
                        let size = buffers[r].size;
                        let usage = resource.buffer_usage;
                        let concurrent = resource.concurrent;
                        let cached = find_from(&slot.buffers, slot.buffer_cursor, |c| {
                            !c.used
                                && c.heap == heap_id
                                && c.offset == item.offset
                                && c.size == size
                                && c.usage == usage
                                && c.concurrent == concurrent
                        });
                        let (buffer, address) = match cached {
                            Some(i) => {
                                slot.buffer_cursor = i + 1;
                                let c = &mut slot.buffers[i];
                                c.used = true;
                                c.last_used = frame;
                                (c.buffer, c.address)
                            }
                            None => {
                                let families = concurrent.then_some(&self.families);
                                // SAFETY: memory and offset come from the heap and placement.
                                let (buffer, address) = unsafe {
                                    create_buffer(
                                        device,
                                        size,
                                        usage,
                                        families,
                                        memory,
                                        base + item.offset,
                                    )
                                }?;
                                name_object(debug_utils, buffer, resource.name);
                                slot.buffers.push(CachedBuffer {
                                    size,
                                    usage,
                                    concurrent,
                                    heap: heap_id,
                                    offset: item.offset,
                                    buffer,
                                    address,
                                    used: true,
                                    last_used: frame,
                                });
                                realized.created_objects += 1;
                                (buffer, address)
                            }
                        };
                        self.current[r] = Physical {
                            buffer,
                            address,
                            ..Physical::default()
                        };
                    }
                }
            }
        }

        // Objects no frame of this slot used for `KEEP_UNUSED` frames: their last use was
        // in an earlier frame of this slot, which the GPU has finished.
        let slot = &mut self.slots[slot_index];
        let kept = |used: bool, last_used: u64| used || frame - last_used < KEEP_UNUSED;
        slot.images.retain_mut(|c| {
            let keep = kept(c.used, c.last_used);
            if !keep {
                // SAFETY: not used by any pending GPU work (see above).
                unsafe { c.destroy(device, bindless) };
            }
            c.used = false;
            keep
        });
        slot.buffers.retain_mut(|c| {
            let keep = kept(c.used, c.last_used);
            if !keep {
                // SAFETY: as above.
                unsafe { device.destroy_buffer(c.buffer, None) };
            }
            c.used = false;
            keep
        });
        Ok(realized)
    }

    /// Destroys every object and frees every heap.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses any of them.
    pub(crate) unsafe fn destroy_all(
        &mut self,
        device: &ash::Device,
        bindless: Option<&Bindless>,
        allocator: &mut Allocator,
    ) {
        for slot in &mut self.slots {
            for c in slot.images.drain(..) {
                // SAFETY: guaranteed by the caller.
                unsafe { c.destroy(device, bindless) };
            }
            for c in slot.buffers.drain(..) {
                // SAFETY: guaranteed by the caller.
                unsafe { device.destroy_buffer(c.buffer, None) };
            }
            for heap in slot.heaps.drain(..) {
                allocator.free(heap.allocation);
            }
        }
        for heap in self.shared.drain(..) {
            allocator.free(heap.allocation);
        }
        for (heap, _) in self.retired.drain(..) {
            allocator.free(heap.allocation);
        }
    }
}

/// A transient heap for `key`'s resources of `size` bytes and more (a quarter of headroom,
/// so that a slowly growing frame does not reallocate often).
fn allocate_heap(
    allocator: &mut Allocator,
    key: GroupKey,
    size: u64,
    align: u64,
    next_id: &mut u64,
) -> Result<Heap, RhiError> {
    let size = (size + size / 4).next_multiple_of(1 << 20);
    let allocation = allocator.allocate(&AllocationRequest {
        requirements: vk::MemoryRequirements {
            size,
            alignment: align,
            memory_type_bits: key.type_bits,
        },
        location: MemoryLocation::Device,
        kind: match key.kind {
            Kind::Texture => ResourceKind::Image,
            Kind::Buffer => ResourceKind::Buffer,
        },
        prefers_dedicated: false,
        requires_dedicated: false,
        dedicated_to: None,
    })?;
    let id = *next_id;
    *next_id += 1;
    Ok(Heap {
        key,
        allocation,
        size,
        id,
    })
}

/// Names a new object after the graph resource it was created for (RenderDoc, Nsight,
/// validation messages). Creation is rare, so the name's allocation does not matter.
fn name_object<H: vk::Handle>(
    utils: Option<&ash::ext::debug_utils::Device>,
    handle: H,
    name: &str,
) {
    let Some(utils) = utils else { return };
    let Ok(name) = std::ffi::CString::new(name) else {
        return;
    };
    let info = vk::DebugUtilsObjectNameInfoEXT::default()
        .object_handle(handle)
        .object_name(&name);
    // SAFETY: a live handle of this device; the name is copied by the call.
    let _ = unsafe { utils.set_debug_utils_object_name(&info) };
}

/// Destroys the cached objects bound to heap `id`.
///
/// # Safety
///
/// The GPU no longer uses them.
unsafe fn destroy_heap_objects(
    device: &ash::Device,
    bindless: Option<&Bindless>,
    slot: &mut Slot,
    id: u64,
) {
    slot.images.retain(|c| {
        if c.heap == id {
            // SAFETY: guaranteed by the caller.
            unsafe { c.destroy(device, bindless) };
        }
        c.heap != id
    });
    slot.buffers.retain(|c| {
        if c.heap == id {
            // SAFETY: guaranteed by the caller.
            unsafe { device.destroy_buffer(c.buffer, None) };
        }
        c.heap != id
    });
}

/// # Safety
///
/// `memory` at `offset` is large enough for the image and allowed by its requirements.
unsafe fn create_image(
    device: &ash::Device,
    desc: &TextureDesc,
    usage: vk::ImageUsageFlags,
    families: Option<&[u32; 2]>,
    memory: vk::DeviceMemory,
    offset: u64,
) -> Result<(vk::Image, vk::ImageView, [vk::ImageView; MAX_LAYER_VIEWS]), RhiError> {
    // SAFETY: valid create info.
    let image = unsafe { device.create_image(&image_info(desc, usage, families), None) }.map_err(
        |result| RhiError::Vulkan {
            call: "vkCreateImage",
            result,
        },
    )?;
    // SAFETY: guaranteed by the caller.
    if let Err(result) = unsafe { device.bind_image_memory(image, memory, offset) } {
        // SAFETY: never used.
        unsafe { device.destroy_image(image, None) };
        return Err(RhiError::Vulkan {
            call: "vkBindImageMemory",
            result,
        });
    }
    // Views need a usage that reads or writes through a view; transfer-only images have
    // none.
    let viewable = vk::ImageUsageFlags::SAMPLED
        | vk::ImageUsageFlags::STORAGE
        | vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT
        | vk::ImageUsageFlags::INPUT_ATTACHMENT;
    let mut layer_views = [vk::ImageView::null(); MAX_LAYER_VIEWS];
    if !usage.intersects(viewable) {
        return Ok((image, vk::ImageView::null(), layer_views));
    }
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(if desc.layers > 1 || desc.array {
            vk::ImageViewType::TYPE_2D_ARRAY
        } else {
            vk::ImageViewType::TYPE_2D
        })
        .format(desc.format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: aspect_of(desc.format),
            base_mip_level: 0,
            level_count: vk::REMAINING_MIP_LEVELS,
            base_array_layer: 0,
            layer_count: vk::REMAINING_ARRAY_LAYERS,
        });
    let failed = |result, views: &[vk::ImageView]| {
        // SAFETY: never used.
        unsafe {
            for &view in views {
                device.destroy_image_view(view, None);
            }
            device.destroy_image(image, None);
        }
        RhiError::Vulkan {
            call: "vkCreateImageView",
            result,
        }
    };
    // SAFETY: the image is bound to memory.
    let view = unsafe { device.create_image_view(&view_info, None) }.map_err(|r| failed(r, &[]))?;
    // A view of each layer an attachment of layers (an array, even of one) is rendered
    // into one by one.
    let attachment =
        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT;
    if (desc.layers > 1 || desc.array) && usage.intersects(attachment) {
        for layer in 0..(desc.layers as usize).min(MAX_LAYER_VIEWS) {
            let info = view_info
                .view_type(vk::ImageViewType::TYPE_2D)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: aspect_of(desc.format),
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: layer as u32,
                    layer_count: 1,
                });
            // SAFETY: as above.
            match unsafe { device.create_image_view(&info, None) } {
                Ok(v) => layer_views[layer] = v,
                Err(r) => {
                    let mut made = layer_views;
                    made[layer] = view;
                    return Err(failed(r, &made[..=layer]));
                }
            }
        }
    }
    Ok((image, view, layer_views))
}

/// # Safety
///
/// `memory` at `offset` is large enough for the buffer and allowed by its requirements,
/// and was allocated with `DEVICE_ADDRESS`.
unsafe fn create_buffer(
    device: &ash::Device,
    size: u64,
    usage: vk::BufferUsageFlags,
    families: Option<&[u32; 2]>,
    memory: vk::DeviceMemory,
    offset: u64,
) -> Result<(vk::Buffer, vk::DeviceAddress), RhiError> {
    // SAFETY: valid create info.
    let buffer = unsafe { device.create_buffer(&buffer_info(size, usage, families), None) }
        .map_err(|result| RhiError::Vulkan {
            call: "vkCreateBuffer",
            result,
        })?;
    // SAFETY: guaranteed by the caller.
    if let Err(result) = unsafe { device.bind_buffer_memory(buffer, memory, offset) } {
        // SAFETY: never used.
        unsafe { device.destroy_buffer(buffer, None) };
        return Err(RhiError::Vulkan {
            call: "vkBindBufferMemory",
            result,
        });
    }
    // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
    let address = unsafe {
        device.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
    };
    Ok((buffer, address))
}
