//! Process-wide settings.

use std::io;
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

#[cfg(test)]
mod tests {
    #[test]
    fn power_throttling_can_be_disabled() {
        super::disable_power_throttling().unwrap();
    }
}
