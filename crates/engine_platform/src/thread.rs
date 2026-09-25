//! Per-thread scheduling controls: CPU sets, priority, MMCSS, description.
//!
//! All functions act on the calling thread, so they fit the job pool's
//! `on_thread_start` hook and the start of the render and audio threads.

use std::io;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentThread,
    SetThreadDescription, SetThreadPriority, SetThreadSelectedCpuSets, THREAD_PRIORITY,
    THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_HIGHEST,
    THREAD_PRIORITY_IDLE, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
    THREAD_PRIORITY_TIME_CRITICAL,
};

/// Restricts the calling thread to the given CPU sets (soft: the scheduler prefers them).
/// An empty slice removes the restriction. Ids come from [`crate::cpu::CpuTopology`].
pub fn set_current_thread_cpu_sets(ids: &[u32]) -> io::Result<()> {
    let count =
        u32::try_from(ids.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let ptr = if ids.is_empty() {
        core::ptr::null()
    } else {
        ids.as_ptr()
    };
    // SAFETY: `ptr` points to `count` ids (or is null with count 0); the pseudo-handle
    // from `GetCurrentThread` is always valid.
    let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), ptr, count) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Thread priority within the process priority class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPriority {
    /// Runs only when nothing else wants the core.
    Idle,
    /// Two steps below normal.
    Lowest,
    /// One step below normal (background work: asset import, shader compilation).
    BelowNormal,
    /// Default.
    Normal,
    /// One step above normal.
    AboveNormal,
    /// Two steps above normal (render thread).
    Highest,
    /// Top of the dynamic range (audio thread; prefer MMCSS).
    TimeCritical,
}

impl ThreadPriority {
    fn raw(self) -> THREAD_PRIORITY {
        match self {
            Self::Idle => THREAD_PRIORITY_IDLE,
            Self::Lowest => THREAD_PRIORITY_LOWEST,
            Self::BelowNormal => THREAD_PRIORITY_BELOW_NORMAL,
            Self::Normal => THREAD_PRIORITY_NORMAL,
            Self::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL,
            Self::Highest => THREAD_PRIORITY_HIGHEST,
            Self::TimeCritical => THREAD_PRIORITY_TIME_CRITICAL,
        }
    }
}

/// Sets the calling thread's priority.
pub fn set_current_thread_priority(priority: ThreadPriority) -> io::Result<()> {
    // SAFETY: pseudo-handle of the calling thread, valid priority constant.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), priority.raw()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Names the calling thread for debuggers, ETW and crash dumps. Allocates (UTF-16
/// conversion): call once at thread start.
pub fn set_current_thread_description(name: &str) -> io::Result<()> {
    let wide = crate::wide(name);
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let hr = unsafe { SetThreadDescription(GetCurrentThread(), wide.as_ptr()) };
    if hr < 0 {
        return Err(io::Error::from_raw_os_error(hr));
    }
    Ok(())
}

/// Multimedia Class Scheduler Service task (CLAUDE.md §5: audio uses "Pro Audio", render
/// uses "Games").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmcssTask {
    /// `Games`: for the render thread.
    Games,
    /// `Pro Audio`: for the audio thread.
    ProAudio,
}

impl MmcssTask {
    fn name(self) -> &'static str {
        match self {
            Self::Games => "Games",
            Self::ProAudio => "Pro Audio",
        }
    }
}

/// The calling thread's MMCSS registration; reverted on drop. Not `Send`: it must be
/// reverted by the thread that registered.
#[derive(Debug)]
pub struct MmcssRegistration {
    handle: HANDLE,
}

impl MmcssRegistration {
    /// Registers the calling thread with MMCSS for `task`.
    pub fn join(task: MmcssTask) -> io::Result<Self> {
        let wide = crate::wide(task.name());
        let mut index = 0u32;
        // SAFETY: `wide` is a NUL-terminated task name; `index` is a valid out pointer.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(wide.as_ptr(), &mut index) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }
}

impl Drop for MmcssRegistration {
    fn drop(&mut self) {
        // SAFETY: the handle came from `AvSetMmThreadCharacteristicsW` on this thread
        // (the type is not `Send`) and is reverted once.
        unsafe { AvRevertMmThreadCharacteristics(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::CpuTopology;

    #[test]
    fn cpu_set_placement_round_trip() {
        let topology = CpuTopology::query().unwrap();
        std::thread::spawn(move || {
            set_current_thread_cpu_sets(&topology.performance_set_ids()).unwrap();
            set_current_thread_cpu_sets(&[]).unwrap();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn priority_and_description() {
        std::thread::spawn(|| {
            set_current_thread_description("engine-platform-test").unwrap();
            set_current_thread_priority(ThreadPriority::AboveNormal).unwrap();
            set_current_thread_priority(ThreadPriority::Normal).unwrap();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn mmcss_registration() {
        std::thread::spawn(|| {
            // MMCSS may be unavailable on server SKUs (CI runners); on client Windows it
            // must work.
            match MmcssRegistration::join(MmcssTask::Games) {
                Ok(registration) => drop(registration),
                Err(e) => eprintln!("MMCSS unavailable: {e}"),
            }
        })
        .join()
        .unwrap();
    }
}
