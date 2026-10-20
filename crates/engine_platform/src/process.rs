//! Process-wide settings, and jobs: a process and every process it starts, waited on
//! together.

use std::io;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JobObjectBasicAccountingInformation, QueryInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
};

/// Opts the process out of power throttling (CLAUDE.md §5).
///
/// - `EXECUTION_SPEED`: Windows would otherwise run a background or occluded process at
///   efficiency QoS (on hybrid CPUs, on E-cores at low clocks).
/// - `IGNORE_TIMER_RESOLUTION`: Windows 11 would otherwise ignore the process's timer
///   resolution requests while its window is minimized or occluded.
///
/// Setting a flag in the control mask while clearing it in the state mask means "never
/// throttle this".
pub fn disable_power_throttling() -> io::Result<()> {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    // SAFETY: `state` is a valid `PROCESS_POWER_THROTTLING_STATE` and outlives the call;
    // the size matches the information class.
    let ok = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            core::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A job object: processes put in it and every process they start, counted together.
/// `ofeliaengine edit` waits on one until the editor and the editors its restarts start
/// have all exited (ADR 0107 §1).
#[derive(Debug)]
pub struct Job {
    handle: HANDLE,
}

// SAFETY: a job object's handle may be used from any thread.
unsafe impl Send for Job {}

impl Job {
    /// An empty job.
    ///
    /// # Errors
    ///
    /// When Windows cannot make one.
    pub fn new() -> io::Result<Self> {
        // SAFETY: no security attributes and no name are allowed.
        let handle = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    /// Puts a started process in the job; the processes it starts from then on join it.
    ///
    /// # Errors
    ///
    /// When the process has exited or cannot join.
    pub fn assign(&self, process: &std::process::Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: both handles are live: the job's is owned here, the process's by
        // `process` for the call.
        let ok = unsafe { AssignProcessToJobObject(self.handle, process.as_raw_handle()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// How many processes of the job still run.
    ///
    /// # Errors
    ///
    /// When Windows cannot tell.
    pub fn active_processes(&self) -> io::Result<u32> {
        // SAFETY: plain data; all zeros is a valid value.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { core::mem::zeroed() };
        // SAFETY: `info` is the structure the information class names, of the size given;
        // it outlives the call.
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                core::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the job's handle, owned here and closed once. The processes in it keep
        // running.
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn power_throttling_can_be_disabled() {
        super::disable_power_throttling().unwrap();
    }

    #[test]
    fn a_job_counts_its_processes_and_their_children() {
        let job = Job::new().unwrap();
        assert_eq!(job.active_processes().unwrap(), 0);
        // A shell that starts a second process, both in the job.
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 2 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        job.assign(&child).unwrap();
        assert!(job.active_processes().unwrap() >= 1);
        child.wait().unwrap();
        // The ping cmd started may outlive cmd itself for a moment.
        let timer = crate::time::HighResTimer::new().unwrap();
        for _ in 0..200 {
            if job.active_processes().unwrap() == 0 {
                break;
            }
            timer.sleep(std::time::Duration::from_millis(10)).unwrap();
        }
        assert_eq!(job.active_processes().unwrap(), 0);
    }
}
