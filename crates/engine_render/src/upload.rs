//! Per-frame GPU data written by the CPU: camera and light constants, draw lists, indirect
//! draw commands. The GPU counterpart of the CPU frame arena (CLAUDE.md §5): one buffer
//! per frame slot, in memory the GPU reads directly (device-local and host-visible where
//! available), reset when the slot comes around again. Allocations return device addresses
//! that shaders read through pointers, so no descriptors are involved.

use core::mem::MaybeUninit;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Device, MemoryLocation, RhiError};

struct RingSlot {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    offset: u64,
}

/// Bump allocation of GPU-visible memory, one ring per frame in flight.
pub struct UploadRing {
    slots: Vec<RingSlot>,
    capacity: u64,
    current: usize,
}

impl core::fmt::Debug for UploadRing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UploadRing")
            .field("slots", &self.slots.len())
            .field("capacity", &self.capacity)
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

impl UploadRing {
    /// `frames_in_flight` buffers of `capacity` bytes each.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        frames_in_flight: usize,
        capacity: u64,
    ) -> Result<Self, RhiError> {
        let mut ring = Self {
            slots: Vec::with_capacity(frames_in_flight),
            capacity,
            current: 0,
        };
        for _ in 0..frames_in_flight.max(1) {
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
                device.raw().get_buffer_device_address(
                    &vk::BufferDeviceAddressInfo::default().buffer(buffer),
                )
            };
            ring.slots.push(RingSlot {
                buffer,
                allocation: Some(allocation),
                address,
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
    /// be done with the slot's previous data.
    pub fn begin_slot(&mut self, slot: usize) {
        self.current = slot % self.slots.len();
        self.slots[self.current].offset = 0;
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
    /// is full. Lets callers write gathered data without an intermediate copy.
    pub fn alloc<T: Copy>(&mut self, count: usize) -> Option<(&mut [MaybeUninit<T>], Staged)> {
        let bytes = (core::mem::size_of::<T>() * count) as u64;
        let slot = &mut self.slots[self.current];
        let align = ALIGN.max(core::mem::align_of::<T>() as u64);
        let offset = slot.offset.next_multiple_of(align);
        if offset + bytes > self.capacity {
            return None;
        }
        let target = slot.allocation.as_mut()?.mapped_slice_mut()?;
        let target = &mut target[offset as usize..(offset + bytes) as usize];
        if !target.as_ptr().cast::<MaybeUninit<T>>().is_aligned() {
            return None;
        }
        slot.offset = offset + bytes;
        let staged = Staged {
            buffer: slot.buffer,
            offset,
            address: slot.address + offset,
        };
        // SAFETY: the range is in bounds, belongs to this allocation until the slot is
        // reset, and is aligned for `T` (checked above). `MaybeUninit` makes any contents
        // valid.
        let values = unsafe {
            core::slice::from_raw_parts_mut(target.as_mut_ptr().cast::<MaybeUninit<T>>(), count)
        };
        Some((values, staged))
    }

    /// Bytes used in the current slot.
    pub fn used(&self) -> u64 {
        self.slots[self.current].offset
    }

    /// Frees the buffers.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        for slot in &mut self.slots {
            if let Some(allocation) = slot.allocation.take() {
                // SAFETY: guaranteed by the caller.
                unsafe { allocator.destroy_buffer(slot.buffer, allocation) };
            }
        }
    }
}
