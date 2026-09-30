//! Batched asynchronous file reads for streaming (CLAUDE.md §13, ADR 0056).
//!
//! Files are opened for unbuffered reads: data goes from the drive straight into the
//! caller's memory, without a copy through the OS file cache (which the engine's own
//! caches make redundant). Unbuffered reads need offsets, sizes and buffer addresses
//! aligned to [`READ_ALIGN`]; [`AlignedBuffer`] provides such memory.
//!
//! A [`ReadQueue`] keeps many reads in flight: on Windows 11 through an I/O ring
//! (`CreateIoRing`), one system call for a batch. Measured on ref-1: 64 KiB reads at random
//! offsets reach 3.7 GB/s on its NVMe drive with 32 in flight against 0.4 GB/s one at a
//! time (29 against 12 MB/s on its hard disk). Where there is no I/O ring (older Windows,
//! other hosts in CI) the queue reads synchronously when a read is pushed.
//!
//! A read takes ownership of its buffer and gives it back with the completion, so no
//! memory can be freed or reused while the drive writes to it:
//!
//! ```no_run
//! use engine_platform::io::{AlignedBuffer, ReadFile, ReadQueue};
//! use std::sync::Arc;
//!
//! let file = Arc::new(ReadFile::open("scene.pack".as_ref())?);
//! let mut queue = ReadQueue::new(32)?;
//! queue.push(&file, 0, AlignedBuffer::new(1 << 20), 7).map_err(|e| e.error)?;
//! queue.submit(true)?;
//! while let Some(done) = queue.pop() {
//!     let bytes = done.result?;
//!     assert_eq!(done.tag, 7);
//!     let data = &done.buffer.as_slice()[..bytes];
//! }
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io;
use std::path::Path;
use std::sync::Arc;

/// Offsets, lengths and buffer addresses of unbuffered reads are multiples of this: the
/// page size, which covers every drive's sector size (512 or 4096 bytes).
pub const READ_ALIGN: usize = 4096;

/// Memory aligned for unbuffered reads, its length a multiple of [`READ_ALIGN`], zeroed.
pub struct AlignedBuffer {
    ptr: core::ptr::NonNull<u8>,
    len: usize,
}

// SAFETY: the buffer owns its memory exclusively, like a `Vec<u8>`.
unsafe impl Send for AlignedBuffer {}
// SAFETY: shared access only reads.
unsafe impl Sync for AlignedBuffer {}

impl core::fmt::Debug for AlignedBuffer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AlignedBuffer").field("len", &self.len).finish()
    }
}

impl AlignedBuffer {
    /// A zeroed buffer of at least `len` bytes (rounded up to [`READ_ALIGN`]; at least one
    /// block).
    ///
    /// # Panics
    ///
    /// When the system is out of memory.
    pub fn new(len: usize) -> Self {
        let len = len.max(1).next_multiple_of(READ_ALIGN);
        let layout = Self::layout(len);
        // SAFETY: `layout` has a non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let Some(ptr) = core::ptr::NonNull::new(ptr) else {
            std::alloc::handle_alloc_error(layout)
        };
        Self { ptr, len }
    }

    fn layout(len: usize) -> std::alloc::Layout {
        match std::alloc::Layout::from_size_align(len, READ_ALIGN) {
            Ok(layout) => layout,
            // A multiple of the alignment below isize::MAX always fits.
            Err(_) => panic!("buffer of {len} bytes is too large"),
        }
    }

    /// Its length (a multiple of [`READ_ALIGN`]).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Never empty (kept for the `len` convention).
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Its bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `len` bytes allocated and initialized (zeroed, then written by reads).
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Its bytes, writable.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the access exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), Self::layout(self.len)) };
    }
}

/// A file opened for unbuffered reads.
#[derive(Debug)]
pub struct ReadFile {
    inner: imp::File,
    len: u64,
}

impl ReadFile {
    /// Opens `path` for reading (unbuffered on Windows).
    pub fn open(path: &Path) -> io::Result<Self> {
        let (inner, len) = imp::File::open(path)?;
        Ok(Self { inner, len })
    }

    /// Its size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A finished read.
#[derive(Debug)]
pub struct ReadDone {
    /// The tag given to [`ReadQueue::push`].
    pub tag: u64,
    /// The buffer given to [`ReadQueue::push`], holding the data.
    pub buffer: AlignedBuffer,
    /// Bytes read (fewer than asked at the end of the file), or why it failed.
    pub result: io::Result<usize>,
}

/// A read that was not queued, with its buffer.
#[derive(Debug)]
pub struct PushError {
    /// The buffer given to [`ReadQueue::push`].
    pub buffer: AlignedBuffer,
    /// Why: the queue is full ([`io::ErrorKind::WouldBlock`]) or the read is not aligned
    /// ([`io::ErrorKind::InvalidInput`]).
    pub error: io::Error,
}

/// Reads in flight (see the module documentation).
pub struct ReadQueue {
    inner: imp::Queue,
    depth: usize,
    /// Reads pushed and not yet popped.
    pending: usize,
}

impl core::fmt::Debug for ReadQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadQueue")
            .field("backend", &self.backend())
            .field("depth", &self.depth)
            .field("pending", &self.pending)
            .finish()
    }
}

impl ReadQueue {
    /// A queue for up to `depth` reads at a time: an I/O ring when the system has one,
    /// otherwise synchronous reads.
    pub fn new(depth: usize) -> io::Result<Self> {
        let depth = depth.clamp(1, 4096);
        Ok(Self {
            inner: imp::Queue::new(depth)?,
            depth,
            pending: 0,
        })
    }

    /// A queue that reads synchronously: for comparisons and tests.
    pub fn synchronous(depth: usize) -> Self {
        let depth = depth.clamp(1, 4096);
        Self {
            inner: imp::Queue::synchronous(depth),
            depth,
            pending: 0,
        }
    }

    /// `"io ring"` or `"synchronous"`.
    pub fn backend(&self) -> &'static str {
        self.inner.backend()
    }

    /// Reads pushed and not yet popped.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Whether another read fits.
    pub fn has_room(&self) -> bool {
        self.pending < self.depth
    }

    /// Queues a read of `buffer.len()` bytes of `file` at `offset` (a multiple of
    /// [`READ_ALIGN`]); it starts at the next [`Self::submit`]. The buffer comes back with
    /// the completion, tagged `tag`.
    pub fn push(
        &mut self,
        file: &Arc<ReadFile>,
        offset: u64,
        buffer: AlignedBuffer,
        tag: u64,
    ) -> Result<(), PushError> {
        if !offset.is_multiple_of(READ_ALIGN as u64) {
            return Err(PushError {
                buffer,
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "read offset not a multiple of READ_ALIGN",
                ),
            });
        }
        if self.pending >= self.depth {
            return Err(PushError {
                buffer,
                error: io::Error::new(io::ErrorKind::WouldBlock, "read queue full"),
            });
        }
        self.inner.push(file, offset, buffer, tag)?;
        self.pending += 1;
        Ok(())
    }

    /// Starts the queued reads; with `wait`, blocks until at least one read has finished
    /// (unless none is pending).
    pub fn submit(&mut self, wait: bool) -> io::Result<()> {
        self.inner.submit(wait && self.pending > 0)
    }

    /// A finished read, if any.
    pub fn pop(&mut self) -> Option<ReadDone> {
        let done = self.inner.pop()?;
        self.pending -= 1;
        Some(done)
    }
}

impl Drop for ReadQueue {
    fn drop(&mut self) {
        // Buffers in flight belong to the drive until their reads finish.
        while self.pending > 0 {
            if self.inner.submit(true).is_err() {
                break;
            }
            while self.pop().is_some() {}
        }
    }
}

/// Converts an `HRESULT` to an I/O error (Win32 codes keep their meaning).
#[cfg(windows)]
fn hresult_error(hr: i32) -> io::Error {
    const FACILITY_WIN32: i32 = 7;
    if (hr >> 16) & 0x1fff == FACILITY_WIN32 {
        io::Error::from_raw_os_error(hr & 0xffff)
    } else {
        io::Error::other(format!("HRESULT {hr:#010x}"))
    }
}

#[cfg(windows)]
mod imp {
    use super::{AlignedBuffer, PushError, ReadDone, ReadFile, hresult_error};
    use std::collections::VecDeque;
    use std::io;
    use std::path::Path;
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_HANDLE_EOF, ERROR_IO_PENDING, GENERIC_READ, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BuildIoRingReadFile, CloseIoRing, CreateFileW, CreateIoRing, FILE_FLAG_NO_BUFFERING,
        FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, GetFileSizeEx, HIORING, IORING_BUFFER_REF,
        IORING_BUFFER_REF_0, IORING_CQE, IORING_CREATE_FLAGS, IORING_HANDLE_REF,
        IORING_HANDLE_REF_0, IORING_REF_RAW, IORING_VERSION_3, OPEN_EXISTING, PopIoRingCompletion,
        ReadFile as Win32ReadFile, SubmitIoRing,
    };
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    #[derive(Debug)]
    pub(super) struct File {
        handle: HANDLE,
    }

    // SAFETY: a file handle may be used from any thread; reads at explicit offsets do not
    // share a file position.
    unsafe impl Send for File {}
    // SAFETY: as above.
    unsafe impl Sync for File {}

    impl File {
        pub(super) fn open(path: &Path) -> io::Result<(Self, u64)> {
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: `wide` is a NUL-terminated path; no security attributes or template.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ,
                    core::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED,
                    core::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let file = Self { handle };
            let mut size = 0i64;
            // SAFETY: a valid handle and an output location.
            if unsafe { GetFileSizeEx(handle, &mut size) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((file, size as u64))
        }

        /// Reads synchronously (the fallback): the handle is overlapped, so the read is
        /// started and then waited for.
        fn read_at(&self, offset: u64, buffer: &mut AlignedBuffer) -> io::Result<usize> {
            // SAFETY: all-zero is a valid OVERLAPPED; no event (the handle is signaled).
            let mut overlapped: OVERLAPPED = unsafe { core::mem::zeroed() };
            overlapped.Anonymous.Anonymous.Offset = offset as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
            let len = u32::try_from(buffer.len()).map_err(io::Error::other)?;
            // SAFETY: the buffer is aligned, `len` bytes long and outlives the wait below;
            // `overlapped` lives until the read has finished.
            let started = unsafe {
                Win32ReadFile(
                    self.handle,
                    buffer.as_mut_slice().as_mut_ptr(),
                    len,
                    core::ptr::null_mut(),
                    &mut overlapped,
                )
            };
            // SAFETY: reads the calling thread's last error.
            if started == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                return eof_or(io::Error::last_os_error());
            }
            let mut read = 0u32;
            // SAFETY: waits for the read started above.
            if unsafe { GetOverlappedResult(self.handle, &overlapped, &mut read, 1) } == 0 {
                return eof_or(io::Error::last_os_error());
            }
            Ok(read as usize)
        }
    }

    /// A read past the end is not an error: nothing was read.
    fn eof_or(error: io::Error) -> io::Result<usize> {
        if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
            Ok(0)
        } else {
            Err(error)
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            // SAFETY: the handle is owned; reads through it hold an `Arc` of the file.
            unsafe { CloseHandle(self.handle) };
        }
    }

    /// A read in flight on the ring: what comes back with its completion.
    pub(super) struct InFlight {
        buffer: AlignedBuffer,
        tag: u64,
        // Keeps the handle open until the read has finished.
        _file: Arc<ReadFile>,
    }

    pub(super) enum Queue {
        Ring {
            ring: HIORING,
            slots: Vec<Option<InFlight>>,
            free: Vec<usize>,
            done: VecDeque<ReadDone>,
        },
        Sync {
            done: VecDeque<ReadDone>,
        },
    }

    // SAFETY: the ring handle is used through `&mut self` only.
    unsafe impl Send for Queue {}

    impl Queue {
        pub(super) fn new(depth: usize) -> io::Result<Self> {
            let mut ring: HIORING = core::ptr::null_mut();
            let entries = (depth as u32).next_power_of_two();
            // SAFETY: an output location; sizes are powers of two within the limits.
            let hr = unsafe {
                CreateIoRing(
                    IORING_VERSION_3,
                    IORING_CREATE_FLAGS {
                        Required: 0,
                        Advisory: 0,
                    },
                    entries,
                    entries * 2,
                    &mut ring,
                )
            };
            if hr < 0 {
                // No I/O ring (before Windows 11): read synchronously.
                return Ok(Self::synchronous(depth));
            }
            Ok(Self::Ring {
                ring,
                slots: (0..depth).map(|_| None).collect(),
                free: (0..depth).rev().collect(),
                done: VecDeque::with_capacity(depth),
            })
        }

        pub(super) fn synchronous(depth: usize) -> Self {
            Self::Sync {
                done: VecDeque::with_capacity(depth),
            }
        }

        pub(super) fn backend(&self) -> &'static str {
            match self {
                Self::Ring { .. } => "io ring",
                Self::Sync { .. } => "synchronous",
            }
        }

        pub(super) fn push(
            &mut self,
            file: &Arc<ReadFile>,
            offset: u64,
            mut buffer: AlignedBuffer,
            tag: u64,
        ) -> Result<(), PushError> {
            match self {
                Self::Sync { done } => {
                    let result = file.inner.read_at(offset, &mut buffer);
                    done.push_back(ReadDone {
                        tag,
                        buffer,
                        result,
                    });
                    Ok(())
                }
                Self::Ring {
                    ring, slots, free, ..
                } => {
                    let Ok(len) = u32::try_from(buffer.len()) else {
                        return Err(PushError {
                            buffer,
                            error: io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "a read is at most 4 GiB",
                            ),
                        });
                    };
                    let Some(slot) = free.pop() else {
                        return Err(PushError {
                            buffer,
                            error: io::Error::new(io::ErrorKind::WouldBlock, "read queue full"),
                        });
                    };
                    let file_ref = IORING_HANDLE_REF {
                        Kind: IORING_REF_RAW,
                        Handle: IORING_HANDLE_REF_0 {
                            Handle: file.inner.handle,
                        },
                    };
                    let data = IORING_BUFFER_REF {
                        Kind: IORING_REF_RAW,
                        Buffer: IORING_BUFFER_REF_0 {
                            Address: buffer.as_mut_slice().as_mut_ptr().cast(),
                        },
                    };
                    // SAFETY: the buffer's heap memory does not move when the buffer is
                    // stored below, and the slot keeps both it and the file alive until
                    // the completion is popped (or the queue waits for it on drop).
                    let hr = unsafe {
                        BuildIoRingReadFile(*ring, file_ref, data, len, offset, slot, 0)
                    };
                    if hr < 0 {
                        free.push(slot);
                        return Err(PushError {
                            buffer,
                            error: hresult_error(hr),
                        });
                    }
                    slots[slot] = Some(InFlight {
                        buffer,
                        tag,
                        _file: file.clone(),
                    });
                    Ok(())
                }
            }
        }

        pub(super) fn submit(&mut self, wait: bool) -> io::Result<()> {
            let Self::Ring {
                ring,
                slots,
                free,
                done,
            } = self
            else {
                return Ok(());
            };
            // Waiting makes sense only for reads still on the ring: finished ones may be
            // waiting to be popped.
            let wait = wait && done.is_empty() && free.len() < slots.len();
            let mut submitted = 0u32;
            // SAFETY: a live ring; every queued entry's memory is held by its slot.
            let hr = unsafe { SubmitIoRing(*ring, u32::from(wait), u32::MAX, &mut submitted) };
            if hr < 0 {
                return Err(hresult_error(hr));
            }
            loop {
                let mut cqe = IORING_CQE::default();
                // SAFETY: a live ring and an output location; S_FALSE (1) means empty.
                if unsafe { PopIoRingCompletion(*ring, &mut cqe) } != 0 {
                    break;
                }
                let Some(read) = slots.get_mut(cqe.UserData).and_then(Option::take) else {
                    continue;
                };
                free.push(cqe.UserData);
                let result = if cqe.ResultCode < 0 {
                    eof_or(hresult_error(cqe.ResultCode))
                } else {
                    Ok(cqe.Information)
                };
                done.push_back(ReadDone {
                    tag: read.tag,
                    buffer: read.buffer,
                    result,
                });
            }
            Ok(())
        }

        pub(super) fn pop(&mut self) -> Option<ReadDone> {
            match self {
                Self::Ring { done, .. } | Self::Sync { done } => done.pop_front(),
            }
        }
    }

    impl Drop for Queue {
        fn drop(&mut self) {
            if let Self::Ring { ring, .. } = self {
                // SAFETY: `ReadQueue::drop` waited for every read first.
                unsafe { CloseIoRing(*ring) };
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{AlignedBuffer, PushError, ReadDone, ReadFile};
    use std::collections::VecDeque;
    use std::io;
    use std::path::Path;
    use std::sync::Arc;

    #[derive(Debug)]
    pub(super) struct File {
        file: std::fs::File,
    }

    impl File {
        pub(super) fn open(path: &Path) -> io::Result<(Self, u64)> {
            let file = std::fs::File::open(path)?;
            let len = file.metadata()?.len();
            Ok((Self { file }, len))
        }

        fn read_at(&self, offset: u64, buffer: &mut AlignedBuffer) -> io::Result<usize> {
            use std::os::unix::fs::FileExt;
            let target = buffer.as_mut_slice();
            let mut total = 0;
            while total < target.len() {
                match self.file.read_at(&mut target[total..], offset + total as u64)? {
                    0 => break,
                    n => total += n,
                }
            }
            Ok(total)
        }
    }

    pub(super) struct Queue {
        done: VecDeque<ReadDone>,
    }

    impl Queue {
        pub(super) fn new(depth: usize) -> io::Result<Self> {
            Ok(Self::synchronous(depth))
        }

        pub(super) fn synchronous(depth: usize) -> Self {
            Self {
                done: VecDeque::with_capacity(depth),
            }
        }

        pub(super) fn backend(&self) -> &'static str {
            "synchronous"
        }

        pub(super) fn push(
            &mut self,
            file: &Arc<ReadFile>,
            offset: u64,
            mut buffer: AlignedBuffer,
            tag: u64,
        ) -> Result<(), PushError> {
            let result = file.inner.read_at(offset, &mut buffer);
            self.done.push_back(ReadDone {
                tag,
                buffer,
                result,
            });
            Ok(())
        }

        pub(super) fn submit(&mut self, _wait: bool) -> io::Result<()> {
            Ok(())
        }

        pub(super) fn pop(&mut self) -> Option<ReadDone> {
            self.done.pop_front()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file of `blocks` blocks, each filled with its own index.
    fn pattern_file(name: &str, blocks: usize) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "engine_platform_io_{name}_{}.bin",
            std::process::id()
        ));
        let data: Vec<u8> = (0..blocks)
            .flat_map(|b| core::iter::repeat_n(b as u8, READ_ALIGN))
            .collect();
        std::fs::write(&path, data).unwrap();
        path
    }

    fn read_everything(mut queue: ReadQueue, name: &str) {
        const BLOCKS: usize = 200;
        let path = pattern_file(name, BLOCKS);
        let file = Arc::new(ReadFile::open(&path).unwrap());
        assert_eq!(file.len(), (BLOCKS * READ_ALIGN) as u64);
        // Two blocks per read, in reverse order, more reads than the queue holds.
        let mut next = (0..BLOCKS / 2).rev().peekable();
        let mut seen = [false; BLOCKS / 2];
        while next.peek().is_some() || queue.pending() > 0 {
            while queue.has_room() {
                let Some(read) = next.next() else { break };
                let offset = (read * 2 * READ_ALIGN) as u64;
                queue
                    .push(&file, offset, AlignedBuffer::new(2 * READ_ALIGN), read as u64)
                    .unwrap();
            }
            queue.submit(true).unwrap();
            while let Some(done) = queue.pop() {
                let read = done.tag as usize;
                assert_eq!(done.result.unwrap(), 2 * READ_ALIGN);
                let data = done.buffer.as_slice();
                assert!(data[..READ_ALIGN].iter().all(|&b| b == (read * 2) as u8));
                assert!(data[READ_ALIGN..].iter().all(|&b| b == (read * 2 + 1) as u8));
                assert!(!seen[read]);
                seen[read] = true;
            }
        }
        assert!(seen.iter().all(|&s| s));
        // Past the end: nothing read. At the end: what is left.
        queue
            .push(&file, (BLOCKS * READ_ALIGN) as u64, AlignedBuffer::new(1), 1)
            .unwrap();
        queue
            .push(&file, ((BLOCKS - 1) * READ_ALIGN) as u64, AlignedBuffer::new(8192), 2)
            .unwrap();
        let mut results = Vec::new();
        while queue.pending() > 0 {
            queue.submit(true).unwrap();
            while let Some(done) = queue.pop() {
                results.push((done.tag, done.result.unwrap()));
            }
        }
        results.sort_unstable();
        assert_eq!(results, [(1, 0), (2, READ_ALIGN)]);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_arrive_whole_and_tagged() {
        let queue = ReadQueue::new(8).unwrap();
        #[cfg(windows)]
        assert_eq!(queue.backend(), "io ring");
        read_everything(queue, "ring");
    }

    #[test]
    fn the_synchronous_queue_reads_the_same() {
        read_everything(ReadQueue::synchronous(8), "sync");
    }

    #[test]
    fn misaligned_and_excess_reads_are_refused() {
        let path = pattern_file("refuse", 4);
        let file = Arc::new(ReadFile::open(&path).unwrap());
        let mut queue = ReadQueue::new(1).unwrap();
        let refused = queue.push(&file, 100, AlignedBuffer::new(1), 0).unwrap_err();
        assert_eq!(refused.error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(refused.buffer.len(), READ_ALIGN);
        queue.push(&file, 0, AlignedBuffer::new(1), 0).unwrap();
        let full = queue.push(&file, 0, AlignedBuffer::new(1), 1).unwrap_err();
        assert_eq!(full.error.kind(), io::ErrorKind::WouldBlock);
        // Dropping the queue with a read in flight waits for it.
        drop(queue);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn buffers_are_aligned_and_zeroed() {
        for len in [1, 4096, 4097, 1 << 20] {
            let buffer = AlignedBuffer::new(len);
            assert_eq!(buffer.as_slice().as_ptr() as usize % READ_ALIGN, 0);
            assert!(buffer.len() >= len && buffer.len().is_multiple_of(READ_ALIGN));
            assert!(buffer.as_slice().iter().all(|&b| b == 0));
        }
    }
}
