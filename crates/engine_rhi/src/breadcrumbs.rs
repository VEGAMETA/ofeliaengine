//! GPU breadcrumbs (ADR 0061): which render graph pass the GPU was in when it crashed or
//! hung.
//!
//! Before and after every pass the command buffer writes a marker into a small
//! host-visible buffer with `vkCmdWriteBufferMarkerAMD` (`VK_AMD_buffer_marker`, which
//! both AMD and NVIDIA drivers expose): at the top of the pipe when the command processor
//! reaches the pass, at the bottom when all its work has finished. (The
//! synchronization2 variant, `vkCmdWriteBufferMarker2AMD`, writes nothing on AMD's
//! 25.10 driver, so the original one is used.) After a device loss
//! or a hang the CPU reads the buffer and reports, per recorded frame, the last pass the
//! GPU started and the last one it finished; the passes in between were running.
//!
//! On AMD the buffer lives in device-coherent, uncached memory
//! (`VK_AMD_device_coherent_memory`), so marker writes reach memory before the GPU is
//! reset instead of sitting in its caches.
//!
//! Recorders (the render graph) take a [`Track`] per frame slot. A track holds the names
//! of the passes of the frame last recorded into it, behind a sequence counter so a
//! crash report never reads a half-updated list.

use crate::error::RhiError;
use ash::vk;
use core::fmt;
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};
use std::sync::{Arc, Weak};

/// Most tracks (recorded command buffers in flight with markers) per device.
pub const MAX_TRACKS: usize = 64;
/// Passes per frame whose names a track keeps; later passes are reported by index.
pub const MAX_PASSES: usize = 1024;

/// Bytes of marker memory per track: the start and end markers.
const TRACK_BYTES: u64 = 8;

/// Marker value for pass `index` of the frame tagged `frame`.
fn marker(frame: u64, index: u32) -> u32 {
    ((frame as u32 & 0xFFFF) << 16) | ((index + 1) & 0xFFFF)
}

/// The frame tag and pass index a marker names; `None` for a track never written.
fn decode(marker: u32) -> Option<(u32, u32)> {
    let pass = marker & 0xFFFF;
    (pass != 0).then(|| (marker >> 16, pass - 1))
}

struct TrackState {
    /// Odd while a frame is being recorded; even and stable otherwise.
    sequence: AtomicU32,
    /// Frame number of the frame last recorded (0: none yet).
    frame: AtomicU64,
    /// Passes recorded in that frame.
    passes: AtomicU32,
    /// Who records into the track (for reports).
    label: AtomicPtr<u8>,
    label_len: AtomicUsize,
    names: Box<[(AtomicPtr<u8>, AtomicUsize)]>,
}

impl TrackState {
    fn new() -> Self {
        Self {
            sequence: AtomicU32::new(0),
            frame: AtomicU64::new(0),
            passes: AtomicU32::new(0),
            label: AtomicPtr::new(core::ptr::null_mut()),
            label_len: AtomicUsize::new(0),
            names: (0..MAX_PASSES)
                .map(|_| (AtomicPtr::new(core::ptr::null_mut()), AtomicUsize::new(0)))
                .collect(),
        }
    }
}

/// The breadcrumb buffer of a device. Created by [`crate::Device::new`] when the GPU has
/// `VK_AMD_buffer_marker` and breadcrumbs are on (`ENGINE_GPU_BREADCRUMBS`, see
/// [`crate::Device::breadcrumbs`]).
pub struct Breadcrumbs {
    device: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *const u32,
    write_marker: vk::PFN_vkCmdWriteBufferMarkerAMD,
    /// Device-coherent, uncached memory (AMD).
    uncached: bool,
    /// Bit `i` set: track `i` is taken.
    taken: AtomicU64,
    tracks: Box<[TrackState]>,
    /// The device destroyed the buffer (it is going away): nothing may be read.
    destroyed: AtomicBool,
}

// SAFETY: `mapped` points into persistently mapped host-coherent memory owned by this
// object; it is only read (volatile) and the GPU writes it. Everything else is atomics or
// Vulkan handles that may be used from any thread.
unsafe impl Send for Breadcrumbs {}
// SAFETY: as above.
unsafe impl Sync for Breadcrumbs {}

impl fmt::Debug for Breadcrumbs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Breadcrumbs")
            .field("uncached", &self.uncached)
            .field("taken", &self.taken.load(Ordering::Relaxed).count_ones())
            .finish()
    }
}

/// A track taken from [`Breadcrumbs`]: markers of one command buffer at a time (one per
/// frame slot). Returned to the pool when dropped.
pub struct Track {
    breadcrumbs: Arc<Breadcrumbs>,
    index: usize,
}

impl fmt::Debug for Track {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Track").field("index", &self.index).finish()
    }
}

impl Breadcrumbs {
    /// Creates the marker buffer. `uncached`: allocate device-coherent, uncached memory
    /// (`deviceCoherentMemory` was enabled).
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        memory_properties: &vk::PhysicalDeviceMemoryProperties,
        uncached: bool,
    ) -> Result<Self, RhiError> {
        // SAFETY: the device was created with VK_AMD_buffer_marker (caller); the name is
        // NUL-terminated.
        let pointer = unsafe {
            instance.get_device_proc_addr(device.handle(), c"vkCmdWriteBufferMarkerAMD".as_ptr())
        };
        let Some(pointer) = pointer else {
            return Err(RhiError::Vulkan {
                call: "vkGetDeviceProcAddr(vkCmdWriteBufferMarkerAMD)",
                result: vk::Result::ERROR_EXTENSION_NOT_PRESENT,
            });
        };
        // SAFETY: the loader returned the entry point of this exact prototype.
        let write_marker: vk::PFN_vkCmdWriteBufferMarkerAMD =
            unsafe { core::mem::transmute(pointer) };
        let size = TRACK_BYTES * MAX_TRACKS as u64;
        // SAFETY: valid create info.
        let buffer = unsafe {
            device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )
        }
        .map_err(RhiError::vk("vkCreateBuffer(breadcrumbs)"))?;
        // SAFETY: the buffer was just created.
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let amd = vk::MemoryPropertyFlags::DEVICE_COHERENT_AMD
            | vk::MemoryPropertyFlags::DEVICE_UNCACHED_AMD;
        let find = |want: vk::MemoryPropertyFlags, avoid: vk::MemoryPropertyFlags| {
            (0..memory_properties.memory_type_count).find(|&i| {
                let flags = memory_properties.memory_types[i as usize].property_flags;
                requirements.memory_type_bits & (1 << i) != 0
                    && flags.contains(want)
                    && !flags.intersects(avoid)
            })
        };
        // System memory rather than the host-visible part of VRAM: it keeps its contents
        // through a GPU reset.
        let none = vk::MemoryPropertyFlags::empty();
        let (type_index, uncached) = match uncached.then(|| find(host | amd, none)).flatten() {
            Some(i) => (Some(i), true),
            None => (
                find(host, vk::MemoryPropertyFlags::DEVICE_LOCAL).or_else(|| find(host, none)),
                false,
            ),
        };
        let destroy_buffer = || {
            // SAFETY: nothing uses the buffer yet.
            unsafe { device.destroy_buffer(buffer, None) };
        };
        let Some(type_index) = type_index else {
            destroy_buffer();
            return Err(RhiError::NoMemoryType {
                type_bits: requirements.memory_type_bits,
                location: crate::MemoryLocation::Readback,
            });
        };
        // SAFETY: valid allocate info for a memory type the buffer allows.
        let memory = match unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(type_index),
                None,
            )
        } {
            Ok(memory) => memory,
            Err(result) => {
                destroy_buffer();
                return Err(RhiError::Vulkan {
                    call: "vkAllocateMemory(breadcrumbs)",
                    result,
                });
            }
        };
        // SAFETY: fresh memory of a type the buffer allows, bound once at offset 0 and
        // mapped once for the object's lifetime; zeroed so no track holds a marker.
        let mapped = unsafe {
            device
                .bind_buffer_memory(buffer, memory, 0)
                .and_then(|()| {
                    device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                })
                .inspect(|&p| core::ptr::write_bytes(p.cast::<u8>(), 0, size as usize))
        };
        let mapped = match mapped {
            Ok(p) => p.cast::<u32>().cast_const(),
            Err(result) => {
                // SAFETY: nothing uses them.
                unsafe { device.free_memory(memory, None) };
                destroy_buffer();
                return Err(RhiError::Vulkan {
                    call: "vkMapMemory(breadcrumbs)",
                    result,
                });
            }
        };
        Ok(Self {
            device: device.clone(),
            buffer,
            memory,
            mapped,
            write_marker,
            uncached,
            taken: AtomicU64::new(0),
            tracks: (0..MAX_TRACKS).map(|_| TrackState::new()).collect(),
            destroyed: AtomicBool::new(false),
        })
    }

    /// Whether marker writes bypass the GPU's caches (AMD device-coherent memory).
    pub fn uncached(&self) -> bool {
        self.uncached
    }

    /// Takes a free track, labelled `label` in reports. `None` when all
    /// [`MAX_TRACKS`] are taken.
    pub fn track(self: &Arc<Self>, label: &'static str) -> Option<Track> {
        let mut taken = self.taken.load(Ordering::Relaxed);
        loop {
            let free = (!taken).trailing_zeros() as usize;
            if free >= MAX_TRACKS {
                return None;
            }
            match self.taken.compare_exchange_weak(
                taken,
                taken | 1 << free,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let state = &self.tracks[free];
                    state.frame.store(0, Ordering::Relaxed);
                    state.passes.store(0, Ordering::Relaxed);
                    state
                        .label
                        .store(label.as_ptr().cast_mut(), Ordering::Relaxed);
                    state.label_len.store(label.len(), Ordering::Release);
                    return Some(Track {
                        breadcrumbs: self.clone(),
                        index: free,
                    });
                }
                Err(now) => taken = now,
            }
        }
    }

    fn markers(&self, track: usize) -> (u32, u32) {
        // SAFETY: `mapped` covers MAX_TRACKS tracks of two u32s; the GPU writes whole u32s,
        // read volatile because the GPU changes them behind the compiler's back.
        unsafe {
            (
                self.mapped.add(track * 2).read_volatile(),
                self.mapped.add(track * 2 + 1).read_volatile(),
            )
        }
    }

    /// Writes, for every track that recorded a frame, the frame, its passes and how far
    /// the GPU got. Lock-free and allocation-free: usable from crash handlers.
    pub fn write_report(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        if self.destroyed.load(Ordering::Acquire) {
            return writeln!(out, "(device destroyed)");
        }
        let mut any = false;
        for (index, state) in self.tracks.iter().enumerate() {
            if self.taken.load(Ordering::Acquire) & (1 << index) == 0 {
                continue;
            }
            let sequence = state.sequence.load(Ordering::Acquire);
            let frame = state.frame.load(Ordering::Relaxed);
            let passes = state.passes.load(Ordering::Relaxed);
            let label_ptr = state.label.load(Ordering::Relaxed);
            let label_len = state.label_len.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if frame == 0 {
                continue;
            }
            any = true;
            let label = if state.sequence.load(Ordering::Relaxed) == sequence {
                // SAFETY: label pointers and lengths come from `&'static str`s and were
                // read consistently (the sequence did not change).
                unsafe { static_str(label_ptr, label_len) }
            } else {
                "?"
            };
            if sequence & 1 == 1 {
                writeln!(
                    out,
                    "track {index} ({label}): frame {frame} is being recorded (not submitted)"
                )?;
                continue;
            }
            let (start, end) = self.markers(index);
            let tag = frame as u32 & 0xFFFF;
            let started = decode(start).filter(|&(t, _)| t == tag).map(|(_, p)| p);
            let finished = decode(end).filter(|&(t, _)| t == tag).map(|(_, p)| p);
            write!(
                out,
                "track {index} ({label}): frame {frame}, {passes} passes: "
            )?;
            match (started, finished) {
                (None, _) => writeln!(out, "the GPU had not started it")?,
                (Some(_), Some(f)) if f + 1 == passes => {
                    writeln!(out, "the GPU finished every pass")?;
                }
                (Some(s), f) => {
                    write!(out, "the GPU last started pass {s} ")?;
                    self.write_name(out, state, sequence, s)?;
                    match f {
                        Some(f) => {
                            write!(out, " and last finished pass {f} ")?;
                            self.write_name(out, state, sequence, f)?;
                            writeln!(out)?;
                        }
                        None => writeln!(out, " and had finished none")?,
                    }
                }
            }
            let first_running = finished.map_or(0, |f| f + 1);
            if let Some(s) = started
                && s >= first_running
                && finished.map(|f| f + 1) != Some(passes)
            {
                write!(out, "  running:")?;
                for p in first_running..=s.min(first_running + 16) {
                    write!(out, " {p} ")?;
                    self.write_name(out, state, sequence, p)?;
                }
                writeln!(out)?;
            }
        }
        if !any {
            writeln!(out, "no frame recorded with markers")?;
        }
        Ok(())
    }

    fn write_name(
        &self,
        out: &mut dyn fmt::Write,
        state: &TrackState,
        sequence: u32,
        pass: u32,
    ) -> fmt::Result {
        let Some((ptr, len)) = state.names.get(pass as usize) else {
            return write!(out, "'?'");
        };
        let (ptr, len) = (ptr.load(Ordering::Relaxed), len.load(Ordering::Relaxed));
        fence(Ordering::Acquire);
        if state.sequence.load(Ordering::Relaxed) != sequence || ptr.is_null() {
            return write!(out, "'?'");
        }
        // SAFETY: names are `&'static str`s, and this pair was read while the track's
        // sequence stayed the same, so pointer and length belong together.
        write!(out, "'{}'", unsafe { static_str(ptr, len) })
    }

    /// A report section for crash reports that follows this object while it lives.
    pub(crate) fn report_section(this: &Arc<Self>) -> engine_log::report::SectionGuard {
        let weak: Weak<Self> = Arc::downgrade(this);
        engine_log::report::add_section("GPU breadcrumbs", move |out| match weak.upgrade() {
            Some(b) => b.write_report(out),
            None => writeln!(out, "(device destroyed)"),
        })
    }
}

/// # Safety
///
/// `ptr` and `len` come from the same `&'static str`.
unsafe fn static_str(ptr: *const u8, len: usize) -> &'static str {
    if ptr.is_null() {
        return "";
    }
    // SAFETY: guaranteed by the caller.
    unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len)) }
}

impl Breadcrumbs {
    /// Destroys the buffer: the device is about to be destroyed. Tracks still held (by
    /// a render graph that outlives its device, a bug) record nothing afterwards.
    ///
    /// # Safety
    ///
    /// The GPU is idle.
    pub(crate) unsafe fn destroy(&self) {
        if self.destroyed.swap(true, Ordering::AcqRel) {
            return;
        }
        // SAFETY: the GPU is idle (caller); `destroyed` keeps readers and recorders away.
        unsafe {
            self.device.unmap_memory(self.memory);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

impl Track {
    /// Index of the track (reports name it).
    pub fn index(&self) -> usize {
        self.index
    }

    fn state(&self) -> &TrackState {
        &self.breadcrumbs.tracks[self.index]
    }

    /// Starts recording frame `frame` (numbers increase; 0 is reserved) into the track.
    /// The GPU must be done with the frame recorded into it before.
    pub fn begin_frame(&self, frame: u64) {
        let state = self.state();
        let sequence = state.sequence.load(Ordering::Relaxed);
        state
            .sequence
            .store(sequence.wrapping_add(1) | 1, Ordering::Relaxed);
        // Readers that see the new names also see the odd sequence.
        fence(Ordering::Release);
        state.frame.store(frame.max(1), Ordering::Relaxed);
        state.passes.store(0, Ordering::Relaxed);
    }

    /// Records the marker opening pass `index` (0, 1, 2… in the frame) named `name`.
    ///
    /// # Safety
    ///
    /// `cmd` is recording, on a queue that supports the stage (any); between
    /// [`Self::begin_frame`] and [`Self::end_frame`].
    #[inline]
    pub unsafe fn pass_begin(&self, cmd: vk::CommandBuffer, index: u32, name: &'static str) {
        let state = self.state();
        if let Some((ptr, len)) = state.names.get(index as usize) {
            ptr.store(name.as_ptr().cast_mut(), Ordering::Relaxed);
            len.store(name.len(), Ordering::Relaxed);
        }
        let frame = state.frame.load(Ordering::Relaxed);
        if self.breadcrumbs.destroyed.load(Ordering::Relaxed) {
            return;
        }
        // SAFETY: recording command buffer (caller); the offset is inside the buffer and
        // a multiple of 4.
        unsafe {
            (self.breadcrumbs.write_marker)(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                self.breadcrumbs.buffer,
                self.index as u64 * TRACK_BYTES,
                marker(frame, index),
            );
        }
    }

    /// Records the marker closing pass `index`.
    ///
    /// # Safety
    ///
    /// As for [`Self::pass_begin`], after it for this pass.
    #[inline]
    pub unsafe fn pass_end(&self, cmd: vk::CommandBuffer, index: u32) {
        let frame = self.state().frame.load(Ordering::Relaxed);
        if self.breadcrumbs.destroyed.load(Ordering::Relaxed) {
            return;
        }
        // SAFETY: as above.
        unsafe {
            (self.breadcrumbs.write_marker)(
                cmd,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.breadcrumbs.buffer,
                self.index as u64 * TRACK_BYTES + 4,
                marker(frame, index),
            );
        }
    }

    /// Ends the frame: `passes` passes were recorded.
    pub fn end_frame(&self, passes: u32) {
        let state = self.state();
        state.passes.store(passes, Ordering::Relaxed);
        let sequence = state.sequence.load(Ordering::Relaxed);
        state
            .sequence
            .store(sequence.wrapping_add(1) & !1, Ordering::Release);
    }

    /// The last pass the GPU started and the last it finished in the frame last recorded
    /// into this track (`None`: not started / none finished).
    pub fn progress(&self) -> (Option<u32>, Option<u32>) {
        if self.breadcrumbs.destroyed.load(Ordering::Acquire) {
            return (None, None);
        }
        let tag = self.state().frame.load(Ordering::Relaxed) as u32 & 0xFFFF;
        let (start, end) = self.breadcrumbs.markers(self.index);
        let pick = |m: u32| decode(m).filter(|&(t, _)| t == tag).map(|(_, p)| p);
        (pick(start), pick(end))
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        let state = self.state();
        state.frame.store(0, Ordering::Relaxed);
        self.breadcrumbs
            .taken
            .fetch_and(!(1 << self.index), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip() {
        assert_eq!(decode(0), None);
        assert_eq!(decode(marker(0x1_2345, 0)), Some((0x2345, 0)));
        assert_eq!(decode(marker(7, 41)), Some((7, 41)));
    }
}
