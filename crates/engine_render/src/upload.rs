//! Per-frame GPU data written by the CPU: camera and light constants, draw lists, indirect
//! draw commands. The GPU counterpart of the CPU frame arena (CLAUDE.md §5): one buffer
//! per frame slot, in memory the GPU reads directly (device-local and host-visible where
//! available), reset when the slot comes around again. Allocations return device addresses
//! that shaders read through pointers, so no descriptors are involved.
//!
//! **Growth** (core review 2 §3.6): a frame that uses more than three quarters of a slot,
//! or asks for more than it has, makes [`UploadRing::grow`] (the frame renderer calls it
//! every frame) prepare buffers twice the demand for every slot; each slot takes its new
//! buffer when it begins again, once the GPU is done with the old one, so addresses handed
//! out stay valid. A frame that outgrows its slot at once still gets `None` (the view is
//! not drawn) and the next frames have the room. [`UploadRing::stats`] reports the use.

use core::mem::MaybeUninit;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Device, MemoryLocation, RhiError};

/// A slot's buffer.
struct RingBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    capacity: u64,
}

struct RingSlot {
    current: RingBuffer,
    /// A larger buffer made by [`UploadRing::grow`], taken when the slot begins again.
    next: Option<RingBuffer>,
    offset: u64,
}

/// Bump allocation of GPU-visible memory, one ring per frame in flight.
pub struct UploadRing {
    slots: Vec<RingSlot>,
    current: usize,
    /// The capacity new buffers get.
    capacity: u64,
    /// The most bytes a frame used, and the most a frame asked for in vain, since the
    /// last growth.
    high_water: u64,
    wanted: u64,
    /// Buffers replaced at the start of their slot: the GPU is done with them.
    retired: Vec<RingBuffer>,
    stats: RingStats,
}

/// The ring's use ([`UploadRing::stats`]): for overlays and capacity choices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingStats {
    /// Bytes per slot new buffers get.
    pub capacity: u64,
    /// The most bytes a frame used since the ring was made.
    pub high_water: u64,
    /// Allocations that did not fit (their frames were not drawn).
    pub failed: u64,
    /// Times the ring grew.
    pub grown: u32,
}

impl core::fmt::Debug for UploadRing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UploadRing")
            .field("slots", &self.slots.len())
            .field("stats", &self.stats)
            .finish()
    }
}

/// Where [`UploadRing::stage`] and [`UploadRing::alloc`] put data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Staged {
    /// The ring buffer of the current slot.
    pub buffer: vk::Buffer,
    /// Byte offset in it.
    pub offset: u64,
    /// Device address of the data.
    pub address: vk::DeviceAddress,
}

/// Alignment of every allocation (covers every scalar, vector and matrix type).
const ALIGN: u64 = 16;

fn make_buffer(
    device: &Device,
    allocator: &mut Allocator,
    capacity: u64,
) -> Result<RingBuffer, RhiError> {
    let (buffer, allocation) = allocator.create_buffer(
        &vk::BufferCreateInfo::default().size(capacity).usage(
            vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::TRANSFER_SRC
                | vk::BufferUsageFlags::UNIFORM_BUFFER
                | vk::BufferUsageFlags::INDIRECT_BUFFER
                | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
        ),
        MemoryLocation::Dynamic,
    )?;
    // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
    let address = unsafe {
        device
            .raw()
            .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
    };
    Ok(RingBuffer {
        buffer,
        allocation: Some(allocation),
        address,
        capacity,
    })
}

/// # Safety
///
/// The GPU no longer uses the buffer.
unsafe fn destroy_buffer(allocator: &mut Allocator, ring: &mut RingBuffer) {
    if let Some(allocation) = ring.allocation.take() {
        // SAFETY: guaranteed by the caller.
        unsafe { allocator.destroy_buffer(ring.buffer, allocation) };
    }
}

impl UploadRing {
    /// `frames_in_flight` buffers of `capacity` bytes each (they grow when frames need
    /// more, [`Self::grow`]).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        frames_in_flight: usize,
        capacity: u64,
    ) -> Result<Self, RhiError> {
        let mut ring = Self {
            slots: Vec::with_capacity(frames_in_flight),
            current: 0,
            capacity,
            high_water: 0,
            wanted: 0,
            retired: Vec::new(),
            stats: RingStats {
                capacity,
                ..RingStats::default()
            },
        };
        for _ in 0..frames_in_flight.max(1) {
            let current = make_buffer(device, allocator, capacity)?;
            ring.slots.push(RingSlot {
                current,
                next: None,
                offset: 0,
            });
        }
        Ok(ring)
    }

    /// Starts writing the data of `frame` (whose slot's previous frame has finished).
    pub fn begin(&mut self, frame: &engine_rhi::Frame) {
        self.begin_slot(frame.slot());
    }

    /// Like [`Self::begin`] for callers that manage frame slots themselves. The GPU must
    /// be done with the slot's previous data. A larger buffer [`Self::grow`] made for the
    /// slot takes over here.
    pub fn begin_slot(&mut self, slot: usize) {
        self.current = slot % self.slots.len();
        let s = &mut self.slots[self.current];
        if let Some(next) = s.next.take() {
            self.retired.push(core::mem::replace(&mut s.current, next));
        }
        s.offset = 0;
    }

    /// Copies `values` into the current slot and returns their device address, or `None`
    /// when the slot is full.
    ///
    /// `T` must be plain data laid out as the shader expects (`#[repr(C)]`, no
    /// references).
    pub fn push<T: Copy>(&mut self, values: &[T]) -> Option<vk::DeviceAddress> {
        self.stage(values).map(|staged| staged.address)
    }

    /// Like [`Self::push`], also returning the buffer and offset (a copy source for
    /// uploads).
    pub fn stage<T: Copy>(&mut self, values: &[T]) -> Option<Staged> {
        let (target, staged) = self.alloc::<T>(values.len())?;
        // SAFETY: `MaybeUninit<T>` has `T`'s layout; the lengths match.
        let source = unsafe {
            core::slice::from_raw_parts(values.as_ptr().cast::<MaybeUninit<T>>(), values.len())
        };
        target.copy_from_slice(source);
        Some(staged)
    }

    /// Reserves room for `count` values of `T` in the current slot and returns it for the
    /// caller to fill (write only: the memory may be uncached), or `None` when the slot
    /// is full (the ring grows for the next frames, [`Self::grow`]). Lets callers write
    /// gathered data without an intermediate copy.
    pub fn alloc<T: Copy>(&mut self, count: usize) -> Option<(&mut [MaybeUninit<T>], Staged)> {
        let bytes = (core::mem::size_of::<T>() * count) as u64;
        let slot = &mut self.slots[self.current];
        let align = ALIGN.max(core::mem::align_of::<T>() as u64);
        let offset = slot.offset.next_multiple_of(align);
        if offset + bytes > slot.current.capacity {
            self.wanted = self.wanted.max(offset + bytes);
            self.stats.failed += 1;
            return None;
        }
        let target = slot.current.allocation.as_mut()?.mapped_slice_mut()?;
        let target = &mut target[offset as usize..(offset + bytes) as usize];
        if !target.as_ptr().cast::<MaybeUninit<T>>().is_aligned() {
            return None;
        }
        slot.offset = offset + bytes;
        self.high_water = self.high_water.max(slot.offset);
        self.stats.high_water = self.stats.high_water.max(slot.offset);
        let staged = Staged {
            buffer: slot.current.buffer,
            offset,
            address: slot.current.address + offset,
        };
        // SAFETY: the range is in bounds, belongs to this allocation until the slot is
        // reset, and is aligned for `T` (checked above). `MaybeUninit` makes any contents
        // valid.
        let values = unsafe {
            core::slice::from_raw_parts_mut(target.as_mut_ptr().cast::<MaybeUninit<T>>(), count)
        };
        Some((values, staged))
    }

    /// Frees the buffers replaced since the last call and, when a frame used more than
    /// three quarters of a slot or asked for more than it had, makes every slot a buffer
    /// twice that demand (rounded to MiB), taken as each slot begins again. Call once a
    /// frame where an allocator is at hand (the frame renderer does); allocates only when
    /// it grows. Returns whether it grew.
    pub fn grow(&mut self, device: &Device, allocator: &mut Allocator) -> Result<bool, RhiError> {
        for mut old in self.retired.drain(..) {
            // SAFETY: replaced when its slot began again: the GPU is done with it.
            unsafe { destroy_buffer(allocator, &mut old) };
        }
        let demand = self.high_water.max(self.wanted);
        if demand * 4 <= self.capacity * 3 {
            return Ok(false);
        }
        let capacity = (demand * 2)
            .next_multiple_of(1 << 20)
            .max(self.capacity * 2);
        for slot in &mut self.slots {
            let next = make_buffer(device, allocator, capacity)?;
            if let Some(mut stale) = slot.next.replace(next) {
                // SAFETY: never handed out: a slot takes its next buffer when it begins.
                unsafe { destroy_buffer(allocator, &mut stale) };
            }
        }
        self.capacity = capacity;
        self.high_water = 0;
        self.wanted = 0;
        self.stats.capacity = capacity;
        self.stats.grown += 1;
        Ok(true)
    }

    /// Bytes used in the current slot.
    pub fn used(&self) -> u64 {
        self.slots[self.current].offset
    }

    /// Bytes still free in the current slot (each allocation may also skip up to 15
    /// bytes to align).
    pub fn available(&self) -> u64 {
        let s = &self.slots[self.current];
        s.current.capacity.saturating_sub(s.offset)
    }

    /// The ring's use: capacity, the most a frame used, failed allocations, growths.
    pub fn stats(&self) -> RingStats {
        self.stats
    }

    /// Frees the buffers.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        for slot in &mut self.slots {
            // SAFETY: guaranteed by the caller.
            unsafe { destroy_buffer(allocator, &mut slot.current) };
            if let Some(mut next) = slot.next.take() {
                // SAFETY: never handed out.
                unsafe { destroy_buffer(allocator, &mut next) };
            }
        }
        for mut old in self.retired.drain(..) {
            // SAFETY: guaranteed by the caller.
            unsafe { destroy_buffer(allocator, &mut old) };
        }
    }
}
