//! Vulkan objects behind graph resources.
//!
//! Each frame slot (frame in flight) owns its transient heaps: frames in flight run
//! concurrently, so they cannot share transient memory. Transient resources are grouped
//! by kind and allowed memory types, placed with aliasing ([`super::placement`]) and bound
//! at their offsets. The images, buffers and views are cached per slot by
//! (description, usage, heap, offset): while the frame's layout repeats, nothing is
//! created. Objects not used by a frame are destroyed when its slot comes around again,
//! when the GPU is done with them.

use super::compile::{Kind, Records};
use super::placement::{self, Item};
use super::{BufferDesc, TextureDesc, aspect_of};
use engine_rhi::ash::{self, vk};
use engine_rhi::{
    Allocation, AllocationRequest, Allocator, Bindless, MemoryLocation, ResourceKind, RhiError,
};

/// The least alignment of a transient buffer (see `requirements_of`).
const TRANSIENT_BUFFER_ALIGN: u64 = 256;

/// The Vulkan side of one graph resource in the current frame.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Physical {
    pub(crate) image: vk::Image,
    pub(crate) view: vk::ImageView,
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
    pub(crate) heap_bytes: u64,
    pub(crate) placed_bytes: u64,
    pub(crate) unaliased_bytes: u64,
    pub(crate) created_objects: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GroupKey {
    kind: Kind,
    type_bits: u32,
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
    sampled_index: Option<u32>,
    storage_index: Option<u32>,
    used: bool,
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
}

/// The placement of one group in the previous frame of a slot, reused while the frame's
/// shape (sizes, lifetimes, resource order) repeats.
struct CachedPlacement {
    key: GroupKey,
    hash: u128,
    heap_size: u64,
    offsets: Vec<u64>,
    alias_ranges: Vec<(u32, u32)>,
    alias_resources: Vec<u32>,
}

#[derive(Default)]
struct Slot {
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
    /// The GPU has finished every submission that used `slot`'s transient resources.
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
            };
            if !self.groups.contains(&key) {
                self.groups.push(key);
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
            let cache_index = match slot.placements.iter().position(|p| p.key == key) {
                Some(i) => i,
                None => {
                    slot.placements.push(CachedPlacement {
                        key,
                        hash: !hash,
                        heap_size: 0,
                        offsets: Vec::new(),
                        alias_ranges: Vec::new(),
                        alias_resources: Vec::new(),
                    });
                    slot.placements.len() - 1
                }
            };
            let cache = &mut slot.placements[cache_index];
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

            // The heap: reuse, or replace when too small.
            let slot = &mut self.slots[slot_index];
            let existing = slot.heaps.iter().position(|h| h.key == key);
            let heap_index = match existing {
                Some(i) if slot.heaps[i].size >= heap_size => i,
                _ => {
                    if let Some(i) = existing {
                        let old = slot.heaps.swap_remove(i);
                        // SAFETY: the slot's previous work is done (caller).
                        unsafe { destroy_heap_objects(device, bindless, slot, old.id) };
                        allocator.free(old.allocation);
                    }
                    // Headroom so that a slowly growing frame does not reallocate often.
                    let size = (heap_size + heap_size / 4).next_multiple_of(1 << 20);
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
                    let id = self.next_heap_id;
                    self.next_heap_id += 1;
                    slot.heaps.push(Heap {
                        key,
                        allocation,
                        size,
                        id,
                    });
                    slot.heaps.len() - 1
                }
            };
            let heap = &slot.heaps[heap_index];
            let (heap_id, memory, base) =
                (heap.id, heap.allocation.memory(), heap.allocation.offset());
            realized.heap_bytes += heap.size;

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
                                i
                            }
                            None => {
                                let families = concurrent.then_some(&self.families);
                                // SAFETY: memory and offset come from the heap and placement.
                                let (image, view) = unsafe {
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
                                    sampled_index: None,
                                    storage_index: None,
                                    used: true,
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

        // Objects this frame did not use: their last use was in this slot's previous
        // frame, which the GPU has finished.
        let slot = &mut self.slots[slot_index];
        slot.images.retain_mut(|c| {
            let keep = c.used;
            if !keep {
                // SAFETY: not used by any pending GPU work (see above).
                unsafe { c.destroy(device, bindless) };
            }
            c.used = false;
            keep
        });
        slot.buffers.retain_mut(|c| {
            let keep = c.used;
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
    }
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
) -> Result<(vk::Image, vk::ImageView), RhiError> {
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
    if !usage.intersects(viewable) {
        return Ok((image, vk::ImageView::null()));
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
    // SAFETY: the image is bound to memory.
    match unsafe { device.create_image_view(&view_info, None) } {
        Ok(view) => Ok((image, view)),
        Err(result) => {
            // SAFETY: never used.
            unsafe { device.destroy_image(image, None) };
            Err(RhiError::Vulkan {
                call: "vkCreateImageView",
                result,
            })
        }
    }
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
