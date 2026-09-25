//! CPU topology from `GetSystemCpuSetInformation` (CLAUDE.md §5: hybrid CPUs).
//!
//! A *CPU set* is Windows' name for one logical processor as seen by the scheduler. Each
//! carries an efficiency class: on hybrid CPUs (Intel P/E cores, some Zen 5 designs)
//! higher classes are faster cores; on homogeneous CPUs every set has class 0. Threads are
//! steered with [`crate::thread::set_current_thread_cpu_sets`], which is a soft
//! preference the scheduler honors, unlike hard affinity masks.

use std::io;
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError};
use windows_sys::Win32::System::SystemInformation::{
    CpuSetInformation, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// One logical processor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuSet {
    /// Id for [`crate::thread::set_current_thread_cpu_sets`].
    pub id: u32,
    /// Processor group.
    pub group: u16,
    /// Logical processor index within the group.
    pub logical_index: u8,
    /// Physical core index (SMT siblings share it).
    pub core_index: u8,
    /// Last-level cache index (cores sharing an L3 share it).
    pub last_level_cache: u8,
    /// NUMA node.
    pub numa_node: u8,
    /// Efficiency class: higher is faster. All zero on homogeneous CPUs.
    pub efficiency_class: u8,
    /// Scheduling class (a hint for how aggressively the OS schedules on this set).
    pub scheduling_class: u8,
    /// The core is parked by power management right now.
    pub parked: bool,
    /// Reserved for exclusive use by some process.
    pub allocated: bool,
    /// Reserved for real-time use.
    pub realtime: bool,
}

/// All CPU sets of the system.
#[derive(Debug, Clone)]
pub struct CpuTopology {
    sets: Vec<CpuSet>,
}

impl CpuTopology {
    /// Queries the topology for the current process.
    pub fn query() -> io::Result<Self> {
        let mut needed = 0u32;
        // SAFETY: a null buffer with length 0 asks for the required size.
        let ok = unsafe {
            GetSystemCpuSetInformation(
                core::ptr::null_mut(),
                0,
                &mut needed,
                GetCurrentProcess(),
                0,
            )
        };
        if ok == 0 {
            // SAFETY: reads the calling thread's last-error value.
            let error = unsafe { GetLastError() };
            if error != ERROR_INSUFFICIENT_BUFFER {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        // u64 elements keep the buffer 8-byte aligned for the records.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        let mut returned = 0u32;
        // SAFETY: `buffer` has at least `needed` bytes.
        let ok = unsafe {
            GetSystemCpuSetInformation(
                buffer.as_mut_ptr().cast(),
                needed,
                &mut returned,
                GetCurrentProcess(),
                0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let bytes = buffer.as_ptr().cast::<u8>();
        let mut sets = Vec::new();
        let mut offset = 0usize;
        while offset + 8 <= returned as usize {
            // SAFETY: each record starts with its `Size` and `Type` (two u32); the records
            // lie within the `returned` bytes the call wrote.
            let (size, kind) = unsafe {
                (
                    bytes.add(offset).cast::<u32>().read_unaligned(),
                    bytes.add(offset + 4).cast::<i32>().read_unaligned(),
                )
            };
            if size == 0 {
                break;
            }
            if kind == CpuSetInformation
                && offset + core::mem::size_of::<SYSTEM_CPU_SET_INFORMATION>() <= returned as usize
            {
                // SAFETY: the record is at least a full `SYSTEM_CPU_SET_INFORMATION` and
                // its type says the `CpuSet` union member is active.
                let info = unsafe {
                    bytes
                        .add(offset)
                        .cast::<SYSTEM_CPU_SET_INFORMATION>()
                        .read_unaligned()
                };
                // SAFETY: `Type == CpuSetInformation` selects the `CpuSet` member; the
                // flag and scheduling-class unions are plain bytes.
                let (cpu, flags, scheduling_class) = unsafe {
                    let cpu = info.Anonymous.CpuSet;
                    (cpu, cpu.Anonymous1.AllFlags, cpu.Anonymous2.SchedulingClass)
                };
                sets.push(CpuSet {
                    id: cpu.Id,
                    group: cpu.Group,
                    logical_index: cpu.LogicalProcessorIndex,
                    core_index: cpu.CoreIndex,
                    last_level_cache: cpu.LastLevelCacheIndex,
                    numa_node: cpu.NumaNodeIndex,
                    efficiency_class: cpu.EfficiencyClass,
                    scheduling_class,
                    parked: flags & 0x1 != 0,
                    allocated: flags & 0x2 != 0,
                    realtime: flags & 0x8 != 0,
                });
            }
            offset += size as usize;
        }
        Ok(Self { sets })
    }

    /// Builds a topology from explicit sets (tests, simulations of other machines).
    pub fn from_sets(sets: Vec<CpuSet>) -> Self {
        Self { sets }
    }

    /// All CPU sets (logical processors).
    pub fn sets(&self) -> &[CpuSet] {
        &self.sets
    }

    /// Highest efficiency class present: the fastest cores.
    pub fn max_efficiency_class(&self) -> u8 {
        self.sets
            .iter()
            .map(|s| s.efficiency_class)
            .max()
            .unwrap_or(0)
    }

    /// Whether the CPU mixes core types (P/E cores).
    pub fn is_hybrid(&self) -> bool {
        self.sets
            .iter()
            .any(|s| s.efficiency_class != self.max_efficiency_class())
    }

    /// Ids of all logical processors on the fastest cores. On homogeneous CPUs: all of
    /// them.
    pub fn performance_set_ids(&self) -> Vec<u32> {
        let class = self.max_efficiency_class();
        self.sets
            .iter()
            .filter(|s| s.efficiency_class == class)
            .map(|s| s.id)
            .collect()
    }

    /// Ids of all logical processors on slower (efficiency) cores. Empty on homogeneous
    /// CPUs.
    pub fn efficiency_set_ids(&self) -> Vec<u32> {
        let class = self.max_efficiency_class();
        self.sets
            .iter()
            .filter(|s| s.efficiency_class != class)
            .map(|s| s.id)
            .collect()
    }

    /// Number of physical cores, optionally only of one efficiency class.
    pub fn physical_cores(&self, efficiency_class: Option<u8>) -> usize {
        let mut cores: Vec<(u16, u8)> = self
            .sets
            .iter()
            .filter(|s| efficiency_class.is_none_or(|c| s.efficiency_class == c))
            .map(|s| (s.group, s.core_index))
            .collect();
        cores.sort_unstable();
        cores.dedup();
        cores.len()
    }

    /// Number of distinct last-level caches (CCDs/CCXs on AMD, clusters on Intel).
    pub fn last_level_caches(&self) -> usize {
        let mut caches: Vec<u8> = self.sets.iter().map(|s| s.last_level_cache).collect();
        caches.sort_unstable();
        caches.dedup();
        caches.len()
    }

    /// One-line summary for logs and reports, for example
    /// `16 logical / 8 cores, 1 LLC, homogeneous`.
    pub fn summary(&self) -> String {
        let mut text = format!(
            "{} logical / {} cores, {} LLC",
            self.sets.len(),
            self.physical_cores(None),
            self.last_level_caches()
        );
        if self.is_hybrid() {
            let class = self.max_efficiency_class();
            let p = self.physical_cores(Some(class));
            let e = self.physical_cores(None) - p;
            text += &format!(", hybrid: {p} performance + {e} efficiency cores");
        } else {
            text += ", homogeneous";
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(id: u32, core: u8, class: u8, llc: u8) -> CpuSet {
        CpuSet {
            id,
            group: 0,
            logical_index: id as u8,
            core_index: core,
            last_level_cache: llc,
            numa_node: 0,
            efficiency_class: class,
            scheduling_class: 0,
            parked: false,
            allocated: false,
            realtime: false,
        }
    }

    #[test]
    fn query_returns_the_logical_processors() {
        let topology = CpuTopology::query().unwrap();
        let logical = std::thread::available_parallelism().unwrap().get();
        assert_eq!(topology.sets().len(), logical);
        let mut ids: Vec<u32> = topology.sets().iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), logical, "CPU set ids are unique");
        assert!(topology.physical_cores(None) >= 1);
        assert!(!topology.performance_set_ids().is_empty());
        assert!(!topology.summary().is_empty());
    }

    #[test]
    fn hybrid_topology_is_split_by_efficiency_class() {
        // 2 P-cores with SMT (class 1) + 4 E-cores (class 0), like a small Intel hybrid.
        let topology = CpuTopology::from_sets(vec![
            set(256, 0, 1, 0),
            set(257, 0, 1, 0),
            set(258, 1, 1, 0),
            set(259, 1, 1, 0),
            set(260, 2, 0, 0),
            set(261, 3, 0, 0),
            set(262, 4, 0, 0),
            set(263, 5, 0, 0),
        ]);
        assert!(topology.is_hybrid());
        assert_eq!(topology.max_efficiency_class(), 1);
        assert_eq!(topology.performance_set_ids(), vec![256, 257, 258, 259]);
        assert_eq!(topology.efficiency_set_ids(), vec![260, 261, 262, 263]);
        assert_eq!(topology.physical_cores(Some(1)), 2);
        assert_eq!(topology.physical_cores(None), 6);
        assert_eq!(
            topology.summary(),
            "8 logical / 6 cores, 1 LLC, hybrid: 2 performance + 4 efficiency cores"
        );
    }

    #[test]
    fn homogeneous_topology() {
        let topology =
            CpuTopology::from_sets(vec![set(1, 0, 0, 0), set(2, 0, 0, 0), set(3, 1, 0, 1)]);
        assert!(!topology.is_hybrid());
        assert!(topology.efficiency_set_ids().is_empty());
        assert_eq!(
            topology.summary(),
            "3 logical / 2 cores, 2 LLC, homogeneous"
        );
    }
}
