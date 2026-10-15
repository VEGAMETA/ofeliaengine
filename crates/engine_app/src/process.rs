//! The game process's set-up (CLAUDE.md §5).

use engine_platform::cpu::CpuTopology;
use engine_platform::thread::{MmcssRegistration, MmcssTask, set_current_thread_cpu_sets};

/// What the set-up holds while the player lives; dropped on the thread that made it.
#[derive(Debug, Default)]
pub(crate) struct ProcessSetup {
    /// The frame thread's MMCSS registration ("Games"), reverted on drop.
    _mmcss: Option<MmcssRegistration>,
    /// What could not be done, for the start-up report.
    pub(crate) notes: Vec<String>,
}

impl ProcessSetup {
    /// Sets the process up for a game, from the frame thread:
    ///
    /// - power throttling off: Windows would run an occluded or background game at
    ///   efficiency QoS and ignore its timer resolution (the editor keeps it, ADR 0023);
    /// - the frame thread on performance cores where the CPU has two kinds (an even CPU
    ///   leaves it to the scheduler);
    /// - the frame thread under MMCSS "Games".
    pub(crate) fn new() -> Self {
        let mut notes = Vec::new();
        if let Err(e) = engine_platform::process::disable_power_throttling() {
            notes.push(format!("power throttling stays on: {e}"));
        }
        match CpuTopology::query() {
            Ok(topology) if topology.is_hybrid() => {
                if let Err(e) = set_current_thread_cpu_sets(&topology.performance_set_ids()) {
                    notes.push(format!("the frame thread is not kept on P-cores: {e}"));
                }
            }
            Ok(_) => {}
            Err(e) => notes.push(format!("no CPU topology: {e}")),
        }
        let mmcss = match MmcssRegistration::join(MmcssTask::Games) {
            Ok(registration) => Some(registration),
            Err(e) => {
                notes.push(format!("no MMCSS \"Games\" for the frame thread: {e}"));
                None
            }
        };
        Self {
            _mmcss: mmcss,
            notes,
        }
    }
}
