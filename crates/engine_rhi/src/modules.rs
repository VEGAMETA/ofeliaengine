//! The Vulkan loader and the modules it loads stay loaded for the rest of the process.
//!
//! The loader loads the layers an instance enables and the drivers (ICDs) when the
//! instance is created, and unloads them when it is destroyed; `ash::Entry` unloads
//! `vulkan-1.dll` itself when it is dropped. A process that creates instances one after
//! another (tests, tools, an editor reopening a device) therefore unloads and reloads the
//! validation layer and the driver whenever its last instance goes away. That is not safe:
//! the validation layer (Vulkan SDK 1.4.350) keeps its allocator's (mimalloc's) per-thread
//! state across the unload, and the next copy's queue threads crash in `mi_thread_init`
//! with an access violation on their first allocation; drivers start threads of their own.
//! So the loader is loaded once ([`entry`]), and every module that appears while an
//! instance is created is pinned ([`Pinning`]): loaded until the process exits, like
//! Streamline's DLLs (ADR 0063). Nothing changes for a process with one instance.

use crate::error::RhiError;
use std::sync::OnceLock;

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
