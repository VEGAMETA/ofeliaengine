//! Where and for how long the Vulkan loader, the layers and the drivers live.
//!
//! **Instances are created on one thread that lives as long as the process**
//! ([`on_loader_thread`]). The loader loads an instance's layers and the drivers (ICDs) on
//! the thread that creates the instance, and a library's initialization may remember that
//! thread. The validation layer of the Vulkan SDK 1.4.350 links mimalloc 3.3.2 statically,
//! which takes the thread that loads it for the process's main thread, identified by the
//! address of its TEB. When that thread exits, mimalloc tears the main thread's heap down
//! (`_mi_theap_free` nulls its `tld`); when Windows later places another thread's TEB at
//! the same address, mimalloc takes that thread for the main thread too, and its first
//! allocation in the layer dereferences the torn-down heap (`_mi_subproc`, reading
//! address 0x18). Tests create every instance on a new thread, so the golden tests died
//! with an access violation in about one run in eight. A thread that never exits keeps
//! every library's idea of its main thread alive.
//!
//! **The loader is loaded once ([`entry`]) and every module that appears while an instance
//! is created is pinned ([`Pinning`])**: the layers and drivers stay loaded until the
//! process exits, like Streamline's DLLs (ADR 0063). Otherwise the loader unloads them with
//! the process's last instance and initializes them again with the next one (a process
//! that creates instances one after another: tests, tools, an editor reopening a device).
//!
//! Nothing changes for a game, which creates one instance, except that its creation runs
//! on the loader thread (one hand-off, tens of microseconds, once).

use crate::error::RhiError;
use std::sync::OnceLock;
use std::sync::mpsc;

type Job = Box<dyn FnOnce() + Send>;

/// Runs `f` on the process's loader thread and returns its result; a panic in `f` is
/// resumed on the calling thread. Without a loader thread (it could not be started), on
/// the calling thread.
pub(crate) fn on_loader_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    static LOADER: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
    let loader = LOADER.get_or_init(|| {
        let (jobs, receiver) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name(String::from("vulkan-loader"))
            .spawn(move || {
                // Lives as long as the process: the sender is in a static.
                while let Ok(job) = receiver.recv() {
                    job();
                }
            })
            .ok()
            .map(|_| jobs)
    });
    let Some(jobs) = loader else {
        return f();
    };
    let (done, result) = mpsc::sync_channel(1);
    let job: Job = Box::new(move || {
        let _ = done.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
    });
    if let Err(mpsc::SendError(job)) = jobs.send(job) {
        // The loader thread is gone (it never exits on its own): run it here.
        job();
    }
    match result.recv() {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        // Every job sends its result, panics included (they are caught).
        Err(_) => unreachable!("a loader job ended without a result"),
    }
}

/// The process's Vulkan loader, loaded on first use and never unloaded.
pub(crate) fn entry() -> Result<ash::Entry, RhiError> {
    static ENTRY: OnceLock<ash::Entry> = OnceLock::new();
    if let Some(entry) = ENTRY.get() {
        return Ok(entry.clone());
    }
    // SAFETY: loads `vulkan-1.dll` / `libvulkan.so.1`; the library stays loaded for the
    // rest of the process (the static is never dropped), so the entry points stay valid.
    let entry = unsafe { ash::Entry::load() }.map_err(|e| RhiError::Loader(e.to_string()))?;
    // Two threads may load it at once: the loser's copy is dropped, which only lowers the
    // library's reference count.
    Ok(ENTRY.get_or_init(|| entry).clone())
}

/// The modules loaded before an instance is created; [`Pinning::finish`] pins the ones
/// that appeared since.
pub(crate) struct Pinning {
    #[cfg(windows)]
    before: Vec<usize>,
}

impl Pinning {
    /// Records the modules loaded now.
    pub(crate) fn start() -> Self {
        Self {
            #[cfg(windows)]
            before: windows::modules(),
        }
    }

    /// Pins every module loaded since [`Self::start`] that is still loaded: the layers and
    /// drivers the loader loaded for the instance (and, harmlessly, anything another thread
    /// loaded meanwhile). A module two instances created at once both load is pinned by
    /// whichever window saw it appear, at least one.
    pub(crate) fn finish(self) {
        #[cfg(windows)]
        {
            let mut before = self.before;
            before.sort_unstable();
            for module in windows::modules() {
                if before.binary_search(&module).is_err() {
                    windows::pin(module);
                }
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleHandleExW,
    };
    use windows_sys::Win32::System::ProcessStatus::EnumProcessModules;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// Base addresses of the modules loaded in this process.
    pub(super) fn modules() -> Vec<usize> {
        let mut handles: Vec<HMODULE> = vec![core::ptr::null_mut(); 512];
        loop {
            let bytes = u32::try_from(handles.len() * size_of::<HMODULE>()).unwrap_or(u32::MAX);
            let mut needed = 0u32;
            // SAFETY: `handles` has room for `bytes` bytes; the pseudo handle of the current
            // process needs no closing.
            let ok = unsafe {
                EnumProcessModules(
                    GetCurrentProcess(),
                    handles.as_mut_ptr(),
                    bytes,
                    &mut needed,
                )
            };
            if ok == 0 {
                return Vec::new();
            }
            let count = needed as usize / size_of::<HMODULE>();
            if count <= handles.len() {
                return handles[..count].iter().map(|&h| h as usize).collect();
            }
            handles.resize(count + 64, core::ptr::null_mut());
        }
    }

    /// Keeps the module at `base` loaded until the process exits. A module unloaded since
    /// it was listed is skipped (the call fails: nothing is mapped at its address).
    pub(super) fn pin(base: usize) {
        let mut module: HMODULE = core::ptr::null_mut();
        // SAFETY: `base` is only used as an address to look up; with `FROM_ADDRESS` the
        // call does not dereference it, and `module` is a valid out pointer.
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                base as *const u16,
                &mut module,
            );
        }
    }
}
