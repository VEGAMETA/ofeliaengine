//! Directory change notifications (`ReadDirectoryChangesW`), for hot reload of shaders,
//! scripts and assets (CLAUDE.md §9, §10, §13).
//!
//! A watcher thread keeps one overlapped `ReadDirectoryChangesW` pending per directory tree
//! and waits on it together with a stop event, so it costs nothing while nothing changes
//! and stops promptly. Events go out through a channel; hot reload is a cold path, so the
//! channel may allocate.
//!
//! When the OS notification buffer overflows (a flood of changes), the watcher reports
//! [`FileEventKind::Rescan`]: consumers must re-check everything under the root.

use std::path::{Path, PathBuf};

/// What happened to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    /// Created (or moved in).
    Created,
    /// Deleted (or moved out).
    Removed,
    /// Contents or attributes changed.
    Modified,
    /// Renamed: this is the old name.
    RenamedFrom,
    /// Renamed: this is the new name.
    RenamedTo,
    /// Notifications were lost; re-scan the whole tree.
    Rescan,
}

/// A change under a watched root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvent {
    /// Absolute path (the root itself for [`FileEventKind::Rescan`]).
    pub path: PathBuf,
    /// What happened.
    pub kind: FileEventKind,
}

/// Decodes a buffer of `FILE_NOTIFY_INFORMATION` records (as filled by
/// `ReadDirectoryChangesW`) into events with paths joined to `root`.
///
/// Layout of each record: `NextEntryOffset: u32`, `Action: u32`, `FileNameLength: u32`
/// (bytes), then the UTF-16 name; records are DWORD-aligned and the last has offset 0.
pub(crate) fn parse_notifications(root: &Path, bytes: &[u8], mut emit: impl FnMut(FileEvent)) {
    let mut offset = 0usize;
    loop {
        let Some(header) = bytes.get(offset..offset + 12) else {
            return;
        };
        let word =
            |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
        let (next, action, name_len) = (word(0) as usize, word(4), word(8) as usize);
        let Some(name_bytes) = bytes.get(offset + 12..offset + 12 + name_len) else {
            return;
        };
        let units: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&units);
        let kind = match action {
            1 => Some(FileEventKind::Created),
            2 => Some(FileEventKind::Removed),
            3 => Some(FileEventKind::Modified),
            4 => Some(FileEventKind::RenamedFrom),
            5 => Some(FileEventKind::RenamedTo),
            _ => None,
        };
        if let Some(kind) = kind {
            emit(FileEvent {
                path: root.join(name),
                kind,
            });
        }
        if next == 0 {
            return;
        }
        offset += next;
    }
}

#[cfg(windows)]
pub use watcher::DirectoryWatcher;

#[cfg(windows)]
mod watcher {
    use super::{FileEvent, FileEventKind, parse_notifications};
    use crate::wide;
    use core::ptr::{null, null_mut};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
        FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects,
    };

    /// Buffer for notification records (the documented maximum for network shares is 64 KiB).
    const BUFFER_BYTES: usize = 64 * 1024;

    /// Watches one directory (optionally recursively) on a background thread.
    pub struct DirectoryWatcher {
        thread: Option<JoinHandle<()>>,
        /// Closed (by field drop) only after the thread has been joined.
        stop: OwnedHandle,
        /// `None` when events go to a caller-supplied sink.
        events: Option<mpsc::Receiver<FileEvent>>,
        root: PathBuf,
    }

    impl core::fmt::Debug for DirectoryWatcher {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("DirectoryWatcher")
                .field("root", &self.root)
                .finish()
        }
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: owned handle, closed once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    // SAFETY: kernel handles may be used from any thread.
    unsafe impl Send for OwnedHandle {}

    impl DirectoryWatcher {
        /// Starts watching `root` (and its subdirectories if `recursive`); read events with
        /// [`Self::try_next`] / [`Self::next_timeout`].
        pub fn new(root: &Path, recursive: bool) -> io::Result<Self> {
            let (tx, rx) = mpsc::channel();
            let mut watcher =
                Self::with_sink(root, recursive, move |event| tx.send(event).is_ok())?;
            watcher.events = Some(rx);
            Ok(watcher)
        }

        /// Starts watching `root`, passing every event to `sink` on the watcher thread.
        /// Returning `false` from `sink` stops the watcher. Several watchers can feed one
        /// consumer this way.
        pub fn with_sink(
            root: &Path,
            recursive: bool,
            sink: impl FnMut(FileEvent) -> bool + Send + 'static,
        ) -> io::Result<Self> {
            let root = std::fs::canonicalize(root)?;
            let root = strip_verbatim(&root);
            let wide_root = wide(&root.to_string_lossy());
            // SAFETY: NUL-terminated path; `FILE_FLAG_BACKUP_SEMANTICS` is required to
            // open a directory; overlapped for cancellable waits.
            let directory = unsafe {
                CreateFileW(
                    wide_root.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    null_mut(),
                )
            };
            if directory == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let directory = OwnedHandle(directory);
            // SAFETY: unnamed events: `stop` manual-reset, `io` auto-reset.
            let (stop, io_event) = unsafe {
                (
                    CreateEventW(null(), 1, 0, null()),
                    CreateEventW(null(), 0, 0, null()),
                )
            };
            // Owned immediately so an early return closes whichever one was created.
            let (stop_owned, io_owned) = (OwnedHandle(stop), OwnedHandle(io_event));
            if stop.is_null() || io_event.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread_root = root.clone();
            // Raw handles are not `Send`; the thread only waits on the event, which `self`
            // owns and outlives it.
            let stop_raw = stop as isize;
            // Changes are recorded only once the first read is pending: wait for it, so
            // that nothing written after the constructor returns can be missed.
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let thread = std::thread::Builder::new()
                .name(String::from("engine-fs-watch"))
                .spawn(move || {
                    watch_thread(
                        directory,
                        io_owned,
                        stop_raw,
                        thread_root,
                        recursive,
                        ready_tx,
                        sink,
                    )
                })?;
            match ready_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = thread.join();
                    return Err(e);
                }
                Err(_) => {
                    let _ = thread.join();
                    return Err(io::Error::other(
                        "the watcher thread exited during start-up",
                    ));
                }
            }
            Ok(Self {
                thread: Some(thread),
                stop: stop_owned,
                events: None,
                root,
            })
        }

        /// The watched root (absolute).
        pub fn root(&self) -> &Path {
            &self.root
        }

        /// Returns the next event, if any, without blocking. Always `None` for a watcher
        /// made with [`Self::with_sink`].
        pub fn try_next(&self) -> Option<FileEvent> {
            self.events.as_ref()?.try_recv().ok()
        }

        /// Blocks up to `timeout` for the next event. Returns `None` at once for a
        /// watcher made with [`Self::with_sink`].
        pub fn next_timeout(&self, timeout: std::time::Duration) -> Option<FileEvent> {
            self.events.as_ref()?.recv_timeout(timeout).ok()
        }
    }

    impl Drop for DirectoryWatcher {
        fn drop(&mut self) {
            // SAFETY: the stop event is owned by `self` and still open.
            unsafe { SetEvent(self.stop.0) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// `canonicalize` returns `\\?\C:\...`; `ReadDirectoryChangesW` accepts it, but events
    /// should carry ordinary paths that compare equal to what users type.
    fn strip_verbatim(path: &Path) -> PathBuf {
        let text = path.to_string_lossy();
        match text.strip_prefix(r"\\?\") {
            Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
            _ => path.to_owned(),
        }
    }

    fn watch_thread(
        directory: OwnedHandle,
        io_event: OwnedHandle,
        stop: isize,
        root: PathBuf,
        recursive: bool,
        ready: mpsc::SyncSender<io::Result<()>>,
        mut sink: impl FnMut(FileEvent) -> bool,
    ) {
        let mut ready = Some(ready);
        // u32 elements: records must be DWORD-aligned.
        let mut buffer = vec![0u32; BUFFER_BYTES / 4];
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE;
        loop {
            let mut overlapped = OVERLAPPED {
                hEvent: io_event.0,
                ..Default::default()
            };
            // SAFETY: the buffer and `overlapped` stay alive and unmoved until the operation
            // completes or is cancelled and waited for (below).
            let started = unsafe {
                ReadDirectoryChangesW(
                    directory.0,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 4) as u32,
                    i32::from(recursive),
                    filter,
                    null_mut(),
                    &mut overlapped,
                    None,
                )
            };
            if let Some(ready) = ready.take() {
                let result = if started == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                };
                let _ = ready.send(result);
            }
            if started == 0 {
                return;
            }
            let handles = [io_event.0, stop as HANDLE];
            // SAFETY: both handles are valid events.
            let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
            if woke != WAIT_OBJECT_0 {
                // Stop (or failure): cancel the pending read and wait for it to finish so the
                // kernel no longer writes into `buffer` / `overlapped`.
                let mut ignored = 0u32;
                // SAFETY: cancels our own pending operation, then waits for its completion.
                unsafe {
                    CancelIoEx(directory.0, &overlapped);
                    GetOverlappedResult(directory.0, &overlapped, &mut ignored, 1);
                }
                return;
            }
            let mut transferred = 0u32;
            // SAFETY: the operation has completed (its event was signaled).
            let ok = unsafe { GetOverlappedResult(directory.0, &overlapped, &mut transferred, 0) };
            let sent = if ok == 0 || transferred == 0 {
                // Overflow (or the directory went away): report a rescan.
                sink(FileEvent {
                    path: root.clone(),
                    kind: FileEventKind::Rescan,
                })
            } else {
                // SAFETY: the kernel wrote `transferred` bytes of records into `buffer`.
                let bytes = unsafe {
                    core::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), transferred as usize)
                };
                let mut alive = true;
                parse_notifications(&root, bytes, |event| alive = alive && sink(event));
                alive
            };
            if !sent {
                // The consumer is gone.
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&next.to_le_bytes());
        bytes.extend_from_slice(&action.to_le_bytes());
        bytes.extend_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for u in units {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn parses_chained_records() {
        let first = record(0, 4, "old.slang");
        let first_len = first.len() as u32;
        let mut buffer = record(first_len, 4, "old.slang");
        buffer.extend(record(0, 5, "shaders\\new.slang"));
        let mut events = Vec::new();
        parse_notifications(Path::new("root"), &buffer, |e| events.push(e));
        assert_eq!(
            events,
            vec![
                FileEvent {
                    path: Path::new("root").join("old.slang"),
                    kind: FileEventKind::RenamedFrom
                },
                FileEvent {
                    path: Path::new("root").join("shaders\\new.slang"),
                    kind: FileEventKind::RenamedTo
                },
            ]
        );
    }

    #[test]
    fn truncated_buffer_is_ignored() {
        let buffer = record(0, 3, "a.slang");
        let mut events = Vec::new();
        parse_notifications(Path::new("r"), &buffer[..10], |e| events.push(e));
        parse_notifications(Path::new("r"), &buffer[..buffer.len() - 4], |e| {
            events.push(e)
        });
        assert!(events.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn reports_file_changes() {
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("engine_fs_watch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let watcher = DirectoryWatcher::new(&dir, true).unwrap();
        let file = watcher.root().join("sub").join("shader.slang");
        std::fs::write(&file, "a").unwrap();
        std::fs::write(&file, "ab").unwrap();
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Some(event) = watcher.next_timeout(Duration::from_millis(100)) {
                seen.push(event.clone());
                if event.path == file && event.kind == FileEventKind::Modified {
                    break;
                }
            }
        }
        assert!(
            seen.iter()
                .any(|e| e.path == file && e.kind == FileEventKind::Modified),
            "events: {seen:?}"
        );
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
