//! GPU fault reports (ADR 0061): what the driver and the breadcrumbs say when the device
//! is lost or the GPU stops making progress.
//!
//! Every device registers its diagnostics here. When a Vulkan call returns
//! `VK_ERROR_DEVICE_LOST` ([`crate::RhiError::vk`] notices), or a frame wait times out,
//! the report is assembled once per device:
//!
//! - `VK_EXT_device_fault`: the driver's description, faulting GPU addresses (page
//!   faults, with their kind and precision), vendor fault codes, and the vendor's binary
//!   crash dump when the driver offers one (NVIDIA does), saved next to the report;
//! - the breadcrumbs ([`crate::Breadcrumbs`]): the last render graph pass the GPU started
//!   and the last one it finished, per recorded frame.
//!
//! The report goes to the log (errors, target `engine_rhi::fault`), to a text file in
//! [`engine_log::report::dir`] together with the last log lines, and stays available from
//! [`last_gpu_fault`].

use crate::breadcrumbs::Breadcrumbs;
use ash::vk;
use core::ffi::CStr;
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

static DEVICES: Mutex<Vec<Weak<Diagnostics>>> = Mutex::new(Vec::new());
static LAST: Mutex<Option<Arc<GpuFaultReport>>> = Mutex::new(None);

/// Whether GPU breadcrumbs are recorded when the GPU supports them:
/// `ENGINE_GPU_BREADCRUMBS=0` turns them off, `1` on; on by default (they cost well under
/// a microsecond per frame, see ADR 0061).
pub(crate) fn breadcrumbs_wanted() -> bool {
    std::env::var("ENGINE_GPU_BREADCRUMBS").map_or(true, |v| v.trim() != "0")
}

/// Whether `VK_EXT_device_fault` is enabled when supported (`ENGINE_DEVICE_FAULT=0`
/// turns it off, for measurements).
fn device_fault_wanted() -> bool {
    std::env::var("ENGINE_DEVICE_FAULT").map_or(true, |v| v.trim() != "0")
}

/// Extensions and features the diagnostics enable, queried before the device exists.
pub(crate) struct Features {
    pub(crate) fault: vk::PhysicalDeviceFaultFeaturesEXT<'static>,
    pub(crate) coherent: vk::PhysicalDeviceCoherentMemoryFeaturesAMD<'static>,
    pub(crate) buffer_marker: bool,
}

impl Features {
    pub(crate) fn query(instance: &ash::Instance, physical: vk::PhysicalDevice) -> Self {
        // SAFETY: `physical` belongs to `instance`.
        let extensions =
            unsafe { instance.enumerate_device_extension_properties(physical) }.unwrap_or_default();
        let has = |name: &CStr| {
            extensions
                .iter()
                .any(|e| e.extension_name_as_c_str() == Ok(name))
        };
        let fault_listed = has(ash::ext::device_fault::NAME) && device_fault_wanted();
        let buffer_marker = has(ash::amd::buffer_marker::NAME) && breadcrumbs_wanted();
        let coherent_listed = buffer_marker && has(ash::amd::device_coherent_memory::NAME);
        let mut fault = vk::PhysicalDeviceFaultFeaturesEXT::default();
        let mut coherent = vk::PhysicalDeviceCoherentMemoryFeaturesAMD::default();
        {
            let mut query = vk::PhysicalDeviceFeatures2::default();
            if fault_listed {
                query = query.push_next(&mut fault);
            }
            if coherent_listed {
                query = query.push_next(&mut coherent);
            }
            // SAFETY: the chain holds only structures of extensions the device lists.
            unsafe { instance.get_physical_device_features2(physical, &mut query) };
        }
        // Detach the structures from the query chain before they are chained again.
        fault.p_next = core::ptr::null_mut();
        coherent.p_next = core::ptr::null_mut();
        fault.device_fault_vendor_binary &= fault.device_fault;
        Self {
            fault,
            coherent,
            buffer_marker,
        }
    }

    /// Extensions to enable.
    pub(crate) fn extensions(&self) -> impl Iterator<Item = &'static CStr> {
        [
            (
                self.fault.device_fault == vk::TRUE,
                ash::ext::device_fault::NAME,
            ),
            (self.buffer_marker, ash::amd::buffer_marker::NAME),
            (
                self.coherent.device_coherent_memory == vk::TRUE,
                ash::amd::device_coherent_memory::NAME,
            ),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
    }
}

/// A device's diagnostics: the fault query, the breadcrumbs, a probe for loss.
pub(crate) struct Diagnostics {
    device: ash::Device,
    adapter: String,
    fault: Option<ash::ext::device_fault::Device>,
    vendor_binary: bool,
    /// Never signaled: `vkGetFenceStatus` on it reports a lost device.
    probe: vk::Fence,
    pub(crate) breadcrumbs: Option<Arc<Breadcrumbs>>,
    _section: Option<engine_log::report::SectionGuard>,
    lost_reported: AtomicBool,
    hang_reported: AtomicBool,
}

impl Diagnostics {
    /// Sets up the diagnostics of a new device and registers them for fault reports.
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        adapter: &crate::Adapter,
        features: &Features,
    ) -> Arc<Self> {
        let fault = (features.fault.device_fault == vk::TRUE)
            .then(|| ash::ext::device_fault::Device::new(instance, device));
        let breadcrumbs = if features.buffer_marker {
            match Breadcrumbs::new(
                instance,
                device,
                &adapter.memory,
                features.coherent.device_coherent_memory == vk::TRUE,
            ) {
                Ok(b) => Some(Arc::new(b)),
                Err(e) => {
                    log::warn!("GPU breadcrumbs unavailable: {e}");
                    None
                }
            }
        } else {
            None
        };
        let section = breadcrumbs.as_ref().map(Breadcrumbs::report_section);
        // SAFETY: valid create info; destroyed in `destroy` before the device.
        let probe = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .unwrap_or_default();
        let diagnostics = Arc::new(Self {
            device: device.clone(),
            adapter: adapter.summary(),
            fault,
            vendor_binary: features.fault.device_fault_vendor_binary == vk::TRUE,
            probe,
            breadcrumbs,
            _section: section,
            lost_reported: AtomicBool::new(false),
            hang_reported: AtomicBool::new(false),
        });
        if let Ok(mut devices) = DEVICES.lock() {
            devices.retain(|d| d.strong_count() > 0);
            devices.push(Arc::downgrade(&diagnostics));
        }
        diagnostics
    }

    pub(crate) fn device_fault_enabled(&self) -> bool {
        self.fault.is_some()
    }

    /// Destroys the Vulkan objects; the device is idle and about to be destroyed.
    ///
    /// # Safety
    ///
    /// Nothing uses the device any more.
    pub(crate) unsafe fn destroy(&self) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if self.probe != vk::Fence::null() {
                self.device.destroy_fence(self.probe, None);
            }
            if let Some(b) = &self.breadcrumbs {
                b.destroy();
            }
        }
    }

    fn is_lost(&self) -> bool {
        if self.probe == vk::Fence::null() {
            return true;
        }
        // SAFETY: the fence belongs to this device and lives until `destroy`.
        let status = unsafe { self.device.get_fence_status(self.probe) };
        matches!(status, Err(vk::Result::ERROR_DEVICE_LOST))
    }

    fn collect(&self, hang: bool) -> DeviceFault {
        let mut report = DeviceFault {
            adapter: self.adapter.clone(),
            description: None,
            addresses: Vec::new(),
            vendor: Vec::new(),
            vendor_binary: Vec::new(),
            fault_query: None,
            breadcrumbs: String::new(),
        };
        if let Some(b) = &self.breadcrumbs {
            let _ = b.write_report(&mut report.breadcrumbs);
        }
        if !hang && let Some(fault) = &self.fault {
            // SAFETY: the device is alive (lost devices still answer this query).
            match unsafe { query_fault(fault, self.vendor_binary) } {
                Ok(q) => {
                    report.description = Some(q.0);
                    report.addresses = q.1;
                    report.vendor = q.2;
                    report.vendor_binary = q.3;
                }
                Err(result) => report.fault_query = Some(result),
            }
        }
        report
    }
}

/// Calls `vkGetDeviceFaultInfoEXT` twice: counts, then contents.
///
/// # Safety
///
/// `fault` belongs to a live device.
#[allow(clippy::type_complexity)]
unsafe fn query_fault(
    fault: &ash::ext::device_fault::Device,
    vendor_binary: bool,
) -> Result<(String, Vec<AddressFault>, Vec<VendorFault>, Vec<u8>), vk::Result> {
    let get = fault.fp().get_device_fault_info_ext;
    let mut counts = vk::DeviceFaultCountsEXT::default();
    // SAFETY: first call of the two-call idiom: only the counts are written.
    unsafe { get(fault.device(), &mut counts, core::ptr::null_mut()) }.result()?;
    if !vendor_binary {
        counts.vendor_binary_size = 0;
    }
    let mut addresses =
        vec![vk::DeviceFaultAddressInfoEXT::default(); counts.address_info_count as usize];
    let mut vendor =
        vec![vk::DeviceFaultVendorInfoEXT::default(); counts.vendor_info_count as usize];
    let mut binary = vec![0u8; counts.vendor_binary_size as usize];
    let mut info = vk::DeviceFaultInfoEXT {
        p_address_infos: addresses.as_mut_ptr(),
        p_vendor_infos: vendor.as_mut_ptr(),
        p_vendor_binary_data: if binary.is_empty() {
            core::ptr::null_mut()
        } else {
            binary.as_mut_ptr().cast()
        },
        ..Default::default()
    };
    // SAFETY: the arrays hold as many elements as the counts say.
    let result = unsafe { get(fault.device(), &mut counts, &mut info) };
    // INCOMPLETE: the driver had more than the counts from the first call; keep what came.
    if result != vk::Result::SUCCESS && result != vk::Result::INCOMPLETE {
        return Err(result);
    }
    addresses.truncate(counts.address_info_count as usize);
    vendor.truncate(counts.vendor_info_count as usize);
    binary.truncate(counts.vendor_binary_size as usize);
    let description = info
        .description_as_c_str()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let addresses = addresses
        .iter()
        .map(|a| AddressFault {
            kind: address_kind(a.address_type),
            address: a.reported_address,
            precision: a.address_precision,
        })
        .collect();
    let vendor = vendor
        .iter()
        .map(|v| VendorFault {
            description: v
                .description_as_c_str()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            code: v.vendor_fault_code,
            data: v.vendor_fault_data,
        })
        .collect();
    Ok((description, addresses, vendor, binary))
}

fn address_kind(kind: vk::DeviceFaultAddressTypeEXT) -> &'static str {
    match kind {
        vk::DeviceFaultAddressTypeEXT::NONE => "no fault",
        vk::DeviceFaultAddressTypeEXT::READ_INVALID => "invalid read",
        vk::DeviceFaultAddressTypeEXT::WRITE_INVALID => "invalid write",
        vk::DeviceFaultAddressTypeEXT::EXECUTE_INVALID => "invalid execute",
        vk::DeviceFaultAddressTypeEXT::INSTRUCTION_POINTER_UNKNOWN => {
            "instruction pointer (unrelated to the fault)"
        }
        vk::DeviceFaultAddressTypeEXT::INSTRUCTION_POINTER_INVALID => {
            "instruction pointer of an invalid instruction"
        }
        vk::DeviceFaultAddressTypeEXT::INSTRUCTION_POINTER_FAULT => {
            "instruction pointer of the faulting instruction"
        }
        _ => "unknown kind",
    }
}

/// A GPU address the driver associates with a fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressFault {
    /// What happened there.
    pub kind: &'static str,
    /// The reported address.
    pub address: u64,
    /// The fault is somewhere in the `precision`-byte aligned range around `address`
    /// (a power of two).
    pub precision: u64,
}

/// A vendor-specific fault record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorFault {
    /// The vendor's description.
    pub description: String,
    /// Vendor fault code.
    pub code: u64,
    /// Vendor fault data.
    pub data: u64,
}

/// What one device reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFault {
    /// The adapter ([`crate::Adapter::summary`]).
    pub adapter: String,
    /// The driver's description of the fault (`VK_EXT_device_fault`), if it has the
    /// extension.
    pub description: Option<String>,
    /// Faulting addresses.
    pub addresses: Vec<AddressFault>,
    /// Vendor fault records.
    pub vendor: Vec<VendorFault>,
    /// The vendor's binary crash dump (starts with
    /// `VkDeviceFaultVendorBinaryHeaderVersionOneEXT`), for the vendor's tools.
    pub vendor_binary: Vec<u8>,
    /// Why the fault query failed, if it did.
    pub fault_query: Option<vk::Result>,
    /// The breadcrumb report ([`crate::Breadcrumbs::write_report`]), empty without them.
    pub breadcrumbs: String,
}

/// A device loss or GPU hang, as reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuFaultReport {
    /// The call that failed.
    pub call: &'static str,
    /// `true`: the GPU stopped making progress (a wait timed out) but the device is not
    /// lost (yet); `false`: `VK_ERROR_DEVICE_LOST`.
    pub hang: bool,
    /// One entry per device reported.
    pub devices: Vec<DeviceFault>,
    /// The text report written to disk, if writing it worked.
    pub report_file: Option<PathBuf>,
    /// Vendor binary dumps written next to it.
    pub dump_files: Vec<PathBuf>,
}

impl fmt::Display for GpuFaultReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.hang {
            writeln!(
                f,
                "GPU hang: {} timed out; the GPU stopped making progress",
                self.call
            )?;
        } else {
            writeln!(f, "GPU device lost (in {})", self.call)?;
        }
        for d in &self.devices {
            writeln!(f, "device: {}", d.adapter)?;
            match (&d.description, d.fault_query) {
                (Some(text), _) => writeln!(
                    f,
                    "  driver: {}",
                    if text.is_empty() {
                        "(no description)"
                    } else {
                        text
                    }
                )?,
                (None, Some(result)) => writeln!(f, "  vkGetDeviceFaultInfoEXT failed: {result}")?,
                (None, None) if !self.hang => {
                    writeln!(f, "  no VK_EXT_device_fault on this driver")?;
                }
                (None, None) => {}
            }
            for a in &d.addresses {
                let precision = a.precision.max(1);
                let lower = a.address & !(precision - 1);
                writeln!(
                    f,
                    "  {}: address {:#018x} (somewhere in {:#x}..{:#x})",
                    a.kind,
                    a.address,
                    lower,
                    lower.saturating_add(precision)
                )?;
            }
            for v in &d.vendor {
                writeln!(
                    f,
                    "  vendor fault: {} (code {:#x}, data {:#x})",
                    v.description, v.code, v.data
                )?;
            }
            if !d.vendor_binary.is_empty() {
                writeln!(f, "  vendor crash dump: {} bytes", d.vendor_binary.len())?;
            }
            if !d.breadcrumbs.is_empty() {
                writeln!(f, "  breadcrumbs:")?;
                for line in d.breadcrumbs.lines() {
                    writeln!(f, "    {line}")?;
                }
            }
        }
        if let Some(path) = &self.report_file {
            writeln!(f, "report: {}", path.display())?;
        }
        for path in &self.dump_files {
            writeln!(f, "vendor dump: {}", path.display())?;
        }
        Ok(())
    }
}

/// The most recent device loss or hang this process reported.
pub fn last_gpu_fault() -> Option<Arc<GpuFaultReport>> {
    LAST.lock().ok().and_then(|last| last.clone())
}

/// A Vulkan call returned `VK_ERROR_DEVICE_LOST`.
pub(crate) fn device_lost(call: &'static str) {
    report(call, false);
}

/// Waiting for the GPU timed out.
pub(crate) fn gpu_hang(call: &'static str) {
    report(call, true);
}

fn report(call: &'static str, hang: bool) {
    let devices: Vec<Arc<Diagnostics>> = match DEVICES.lock() {
        Ok(devices) => devices.iter().filter_map(Weak::upgrade).collect(),
        Err(_) => return,
    };
    let several = devices.len() > 1;
    let mut faults = Vec::new();
    for d in &devices {
        // With several devices, report the lost one (the probe says which); a hang is
        // reported for all, as the waiting code does not say which device it waited on.
        if !hang && several && !d.is_lost() {
            continue;
        }
        let flag = if hang {
            &d.hang_reported
        } else {
            &d.lost_reported
        };
        if flag.swap(true, Ordering::AcqRel) {
            continue;
        }
        faults.push(d.collect(hang));
    }
    if faults.is_empty() {
        return;
    }
    let mut report = GpuFaultReport {
        call,
        hang,
        devices: faults,
        report_file: None,
        dump_files: Vec::new(),
    };
    write_files(&mut report);
    let text = report.to_string();
    for line in text.lines() {
        log::error!(target: "engine_rhi::fault", "{line}");
    }
    log::logger().flush();
    if let Ok(mut last) = LAST.lock() {
        *last = Some(Arc::new(report));
    }
}

fn write_files(report: &mut GpuFaultReport) {
    let dir = engine_log::report::dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stem = format!(
        "gpu-{}-{}-{}",
        if report.hang { "hang" } else { "fault" },
        engine_log::UtcTime::now().compact().as_str(),
        std::process::id()
    );
    for (i, d) in report.devices.iter().enumerate() {
        if d.vendor_binary.is_empty() {
            continue;
        }
        let path = dir.join(format!("{stem}-device{i}.bin"));
        if std::fs::write(&path, &d.vendor_binary).is_ok() {
            report.dump_files.push(path);
        }
    }
    let path = dir.join(format!("{stem}.txt"));
    let mut text = report.to_string();
    let _ = writeln!(text, "\nlast log lines:");
    let _ = engine_log::write_recent(&mut text, 300);
    let _ = writeln!(text, "\nreport sections:");
    let _ = engine_log::report::write_sections(&mut text);
    if std::fs::write(&path, text).is_ok() {
        report.report_file = Some(path);
    }
}
