//! Input latency and frame pacing (CLAUDE.md §7, ADR 0023).
//!
//! One interface over three backends:
//! - **NVIDIA Reflex** through the driver extension `VK_NV_low_latency2` (no DLL);
//! - **AMD Anti-Lag 2** through `VK_AMD_anti_lag` (no DLL);
//! - **a generic path on any GPU:** a frame-rate limiter on a high-resolution waitable
//!   timer plus `VK_KHR_present_wait`, which bounds how many presents may be queued.
//!
//! Every frame the game calls, in order:
//! 1. [`Latency::begin_frame`], before sampling input: the backend sleeps just long
//!    enough that the frame finishes as the GPU becomes free;
//! 2. [`Latency::mark`] around simulation, render submission and presentation;
//! 3. [`Latency::before_present`] right before presenting.
//!
//! The markers are shared by all backends, Tracy and the latency overlay.
//!
//! With frame generation a thread of its own presents ([`PacedPresenter`], ADR 0091): the
//! frame loop passes it where it passed the swapchain, and the markers of presents go
//! with it ([`Latency::present_markers`]), for real frames only.
//!
//! While a library presents in the engine's place and requires its own latency control
//! (Streamline's Reflex and PC latency markers with DLSS frame generation, ADR 0091 §6),
//! [`Latency::with_external`] hands the same calls to it ([`ExternalLatency`]).

use engine_rhi::ash::vk;
use engine_rhi::{Device, Frames, RhiError, Swapchain};
use std::time::{Duration, Instant};

mod paced;

pub use paced::{PacedFrame, PacedImages, PacedPresenter, PresentImage, Presented, RING};

/// What latency control needs of the swapchain the frame loop presents to: the swapchain
/// itself, or the [`PacedPresenter`] that presents to it from a thread of its own.
pub trait LatencyTarget {
    /// The swapchain's handle (the Reflex calls take it; they may run on any thread).
    fn raw(&self) -> vk::SwapchainKHR;
    /// Whether it was created for Reflex (`VK_NV_low_latency2`).
    fn low_latency(&self) -> bool;
    /// The largest present id used so far.
    fn last_present_id(&self) -> u64;
    /// Waits until the present tagged `id` is on screen (`VK_KHR_present_wait`).
    fn wait_for_present(&self, id: u64, timeout_ns: u64) -> Result<bool, RhiError>;
    /// Whether [`Self::wait_for_present`] can be called from the frame loop.
    fn can_wait_for_present(&self) -> bool {
        true
    }
}

impl LatencyTarget for Swapchain {
    fn raw(&self) -> vk::SwapchainKHR {
        Swapchain::raw(self)
    }

    fn low_latency(&self) -> bool {
        Swapchain::low_latency(self)
    }

    fn last_present_id(&self) -> u64 {
        Swapchain::last_present_id(self)
    }

    fn wait_for_present(&self, id: u64, timeout_ns: u64) -> Result<bool, RhiError> {
        Swapchain::wait_for_present(self, id, timeout_ns)
    }
}

/// Which implementation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// NVIDIA Reflex via `VK_NV_low_latency2`.
    NvidiaReflex,
    /// AMD Anti-Lag 2 via `VK_AMD_anti_lag`.
    AmdAntiLag,
    /// Limiter plus `present_wait` queue control, on any GPU.
    Generic,
    /// A library's own ([`ExternalLatency`]): Streamline's Reflex with DLSS frame
    /// generation.
    External,
}

/// Latency control a library owns, in place of the driver's extensions: Streamline's
/// Reflex and PC latency markers while DLSS frame generation is loaded, which works with
/// no other Reflex integration (ADR 0091 §6). [`Latency::with_external`] calls it at the
/// points it calls the other backends, with the frame's id.
pub trait ExternalLatency: Send {
    /// Applies `settings` (the mode and the frame-rate cap, which caps real frames).
    fn apply(&mut self, settings: &LatencySettings);
    /// Sleeps until frame `frame` should start, so that input sampled right after is as
    /// fresh as possible. Called every frame, whatever the mode.
    fn sleep(&mut self, frame: u64);
    /// Records `marker` of frame `frame`. Called for every marker, whatever the mode.
    fn mark(&mut self, frame: u64, marker: Marker);
}

/// Latency reduction level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowLatency {
    /// No latency reduction (the frame limiter still applies).
    Off,
    /// Keep the render queue empty.
    On,
    /// Also ask the driver to keep clocks high (Reflex "boost").
    Boost,
}

/// Latency settings, typically from the game's options menu.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencySettings {
    /// Latency reduction level.
    pub mode: LowLatency,
    /// Frame-rate cap. With variable refresh rate, a few frames below the display's
    /// maximum keeps it in range.
    pub fps_limit: Option<f64>,
    /// Generic path: how many frames may be queued beyond the frame being built; 0 does
    /// not wait. Counted in presents waiting to be displayed with `VK_KHR_present_wait`,
    /// otherwise in frames the GPU has not finished.
    pub max_queued_presents: u32,
}

impl Default for LatencySettings {
    fn default() -> Self {
        Self {
            mode: LowLatency::On,
            fps_limit: None,
            max_queued_presents: 1,
        }
    }
}

/// Points in a frame, shared by every backend, Tracy and the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// Input was read for this frame.
    InputSample,
    /// Simulation started.
    SimulationStart,
    /// Simulation finished.
    SimulationEnd,
    /// Recording and submission of GPU work started.
    RenderSubmitStart,
    /// Submission finished.
    RenderSubmitEnd,
    /// `vkQueuePresentKHR` called.
    PresentStart,
    /// `vkQueuePresentKHR` returned.
    PresentEnd,
}

const MARKERS: usize = 7;

impl Marker {
    fn index(self) -> usize {
        self as usize
    }

    fn nv(self) -> vk::LatencyMarkerNV {
        match self {
            Marker::InputSample => vk::LatencyMarkerNV::INPUT_SAMPLE,
            Marker::SimulationStart => vk::LatencyMarkerNV::SIMULATION_START,
            Marker::SimulationEnd => vk::LatencyMarkerNV::SIMULATION_END,
            Marker::RenderSubmitStart => vk::LatencyMarkerNV::RENDERSUBMIT_START,
            Marker::RenderSubmitEnd => vk::LatencyMarkerNV::RENDERSUBMIT_END,
            Marker::PresentStart => vk::LatencyMarkerNV::PRESENT_START,
            Marker::PresentEnd => vk::LatencyMarkerNV::PRESENT_END,
        }
    }
}

/// CPU timestamps of one frame's markers.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameTimes {
    /// The frame's present id (1-based).
    pub frame: u64,
    /// When [`Latency::begin_frame`] returned.
    pub begin: Option<Instant>,
    marks: [Option<Instant>; MARKERS],
}

impl FrameTimes {
    /// When `marker` was recorded in this frame.
    pub fn at(&self, marker: Marker) -> Option<Instant> {
        self.marks[marker.index()]
    }
}

/// Frames of history kept for statistics.
pub const HISTORY: usize = 128;

// `VK_AMD_anti_lag` (Vulkan 1.3.290), newer than the bindings.
const ANTI_LAG_DATA_AMD: vk::StructureType = vk::StructureType::from_raw(1_000_476_001);
const ANTI_LAG_PRESENTATION_INFO_AMD: vk::StructureType =
    vk::StructureType::from_raw(1_000_476_002);
const ANTI_LAG_MODE_ON: i32 = 1;
const ANTI_LAG_MODE_OFF: i32 = 2;
const ANTI_LAG_STAGE_INPUT: i32 = 0;
const ANTI_LAG_STAGE_PRESENT: i32 = 1;

#[repr(C)]
struct AntiLagPresentationInfoAmd {
    s_type: vk::StructureType,
    p_next: *mut core::ffi::c_void,
    stage: i32,
    frame_index: u64,
}

#[repr(C)]
struct AntiLagDataAmd {
    s_type: vk::StructureType,
    p_next: *const core::ffi::c_void,
    mode: i32,
    max_fps: u32,
    presentation_info: *const AntiLagPresentationInfoAmd,
}

type AntiLagUpdateFn = unsafe extern "system" fn(vk::Device, *const AntiLagDataAmd);

struct Nv {
    loader: engine_rhi::ash::nv::low_latency2::Device,
    semaphore: vk::Semaphore,
    value: u64,
    /// When the last sleep ended, and the interval between the last two: the next wait
    /// for the driver's signal is cut at a few such intervals ([`sleep_bound_ns`]).
    woke: Option<Instant>,
    interval_ns: u64,
    /// Sleeps cut at that bound.
    cut: u64,
}

#[derive(Clone, Copy)]
struct Amd {
    update: AntiLagUpdateFn,
    device: vk::Device,
}

impl Amd {
    fn update(&self, on: bool, max_fps: u32, stage: i32, frame: u64) {
        let info = AntiLagPresentationInfoAmd {
            s_type: ANTI_LAG_PRESENTATION_INFO_AMD,
            p_next: core::ptr::null_mut(),
            stage,
            frame_index: frame,
        };
        let data = AntiLagDataAmd {
            s_type: ANTI_LAG_DATA_AMD,
            p_next: core::ptr::null(),
            mode: if on {
                ANTI_LAG_MODE_ON
            } else {
                ANTI_LAG_MODE_OFF
            },
            max_fps,
            presentation_info: if on { &info } else { core::ptr::null() },
        };
        // SAFETY: the function belongs to this device; the structures follow the
        // registry's layout and outlive the call.
        unsafe { (self.update)(self.device, &data) };
    }
}

/// The calls around a real frame's present when another thread presents
/// ([`PacedPresenter`], ADR 0091): Reflex's present markers and Anti-Lag 2's present
/// stage. Generated frames get none: the latency markers are the real frames'.
#[derive(Clone)]
pub struct PresentMarkers {
    nv: Option<(engine_rhi::ash::nv::low_latency2::Device, vk::SwapchainKHR)>,
    amd: Option<Amd>,
    max_fps: u32,
}

// SAFETY: a function table, handles and a device-level function pointer; the Vulkan calls
// they make need no host synchronization (`vkSetLatencyMarkerNV`'s swapchain may be used
// by other threads at the same time, by the spec).
unsafe impl Send for PresentMarkers {}

impl core::fmt::Debug for PresentMarkers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PresentMarkers")
            .field("reflex", &self.nv.is_some())
            .field("anti_lag", &self.amd.is_some())
            .finish()
    }
}

impl PresentMarkers {
    /// Right before the real frame `id`'s `vkQueuePresentKHR`.
    pub fn before_present(&self, id: u64) {
        if let Some(amd) = &self.amd {
            amd.update(true, self.max_fps, ANTI_LAG_STAGE_PRESENT, id);
        }
        self.nv_marker(id, vk::LatencyMarkerNV::PRESENT_START);
    }

    /// Right after it.
    pub fn after_present(&self, id: u64) {
        self.nv_marker(id, vk::LatencyMarkerNV::PRESENT_END);
    }

    fn nv_marker(&self, id: u64, marker: vk::LatencyMarkerNV) {
        if let Some((loader, swapchain)) = &self.nv {
            let info = vk::SetLatencyMarkerInfoNV::default()
                .present_id(id)
                .marker(marker);
            // SAFETY: the swapchain was created with latency mode enabled.
            unsafe { loader.set_latency_marker(*swapchain, &info) };
        }
    }
}

/// Fallback when no waitable timer is available (and in non-Windows builds, which exist
/// only for tests).
fn spin_until(deadline: Instant) {
    while Instant::now() < deadline {
        std::thread::yield_now();
    }
}

#[cfg(windows)]
struct Sleeper(Option<engine_platform::time::PreciseSleeper>);

#[cfg(windows)]
impl Sleeper {
    fn new() -> Self {
        Self(engine_platform::time::PreciseSleeper::new().ok())
    }

    fn sleep_until(&mut self, deadline: Instant) {
        let precise = match &mut self.0 {
            Some(s) => s.sleep_until(deadline).is_ok(),
            None => false,
        };
        if !precise {
            spin_until(deadline);
        }
    }
}

#[cfg(not(windows))]
struct Sleeper;

#[cfg(not(windows))]
impl Sleeper {
    fn new() -> Self {
        Self
    }

    fn sleep_until(&mut self, deadline: Instant) {
        spin_until(deadline);
    }
}

/// How long `begin_frame` waits for the Reflex sleep semaphore before giving up, at most.
const NV_SLEEP_TIMEOUT_NS: u64 = 200_000_000;
/// The least it waits for it once the frame interval is known.
const NV_SLEEP_MIN_BOUND_NS: u64 = 4_000_000;

/// How long to wait for Reflex's signal after a frame interval of `interval_ns` (0: none
/// measured yet): three intervals and a millisecond, within 4 and 200 ms. The driver's
/// sleep is as long as the GPU or the frame-rate cap needs, which the last interval
/// already shows; on ref-1 it now and then signalled 15–43 ms late where frames took
/// 2 ms (ADR 0023, addendum 2026-10-16), and the frame slot still bounds the CPU.
fn sleep_bound_ns(interval_ns: u64) -> u64 {
    if interval_ns == 0 {
        return NV_SLEEP_TIMEOUT_NS;
    }
    interval_ns
        .saturating_mul(3)
        .saturating_add(1_000_000)
        .clamp(NV_SLEEP_MIN_BOUND_NS, NV_SLEEP_TIMEOUT_NS)
}
/// How long the generic path waits for an old present before giving up.
const PRESENT_WAIT_TIMEOUT_NS: u64 = 200_000_000;

/// Latency control for one swapchain. See the module docs for the call order.
pub struct Latency {
    backend: Backend,
    settings: LatencySettings,
    device: engine_rhi::ash::Device,
    present_id: bool,
    /// `VK_KHR_present_wait` with present ids (the generic path waits on presents).
    present_wait: bool,
    nv: Option<Nv>,
    amd: Option<Amd>,
    external: Option<Box<dyn ExternalLatency>>,
    frame: u64,
    next_frame_at: Option<Instant>,
    sleeper: Sleeper,
    history: Box<[FrameTimes; HISTORY]>,
}

impl core::fmt::Debug for Latency {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Latency")
            .field("backend", &self.backend)
            .field("settings", &self.settings)
            .field("frame", &self.frame)
            .finish()
    }
}

impl Latency {
    /// Picks the best backend for `device` and `swapchain`: Reflex when the swapchain was
    /// created with `low_latency` on a device with `VK_NV_low_latency2`, else Anti-Lag 2
    /// when `VK_AMD_anti_lag` is enabled, else the generic path. Applies `settings`.
    pub fn new(
        device: &Device,
        swapchain: &impl LatencyTarget,
        settings: LatencySettings,
    ) -> Result<Self, RhiError> {
        Self::with_backend(device, swapchain, settings, None)
    }

    /// Like [`Latency::new`], preferring `backend` (for comparisons and user settings);
    /// falls back like `new` when it is unavailable. `Some(Backend::Generic)` always
    /// works.
    pub fn with_backend(
        device: &Device,
        swapchain: &impl LatencyTarget,
        settings: LatencySettings,
        backend: Option<Backend>,
    ) -> Result<Self, RhiError> {
        let ext = device.latency_extensions();
        let raw = device.raw();
        let allow_nv = matches!(backend, None | Some(Backend::NvidiaReflex));
        let allow_amd = matches!(
            backend,
            None | Some(Backend::AmdAntiLag) | Some(Backend::NvidiaReflex)
        );
        let nv = if allow_nv && swapchain.low_latency() {
            let mut type_info = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            // SAFETY: valid create info; destroyed in `Drop`.
            let semaphore = unsafe {
                raw.create_semaphore(
                    &vk::SemaphoreCreateInfo::default().push_next(&mut type_info),
                    None,
                )
            }
            .map_err(RhiError::vk("vkCreateSemaphore(latency)"))?;
            Some(Nv {
                loader: engine_rhi::ash::nv::low_latency2::Device::new(device.instance_raw(), raw),
                semaphore,
                value: 0,
                woke: None,
                interval_ns: 0,
                cut: 0,
            })
        } else {
            None
        };
        let amd = if allow_amd && nv.is_none() && ext.amd_anti_lag {
            // SAFETY: querying a device-level function the device enabled.
            let f = unsafe {
                device
                    .instance_raw()
                    .get_device_proc_addr(raw.handle(), c"vkAntiLagUpdateAMD".as_ptr())
            };
            f.map(|f| Amd {
                // SAFETY: `vkAntiLagUpdateAMD` has this signature (Vulkan registry).
                update: unsafe {
                    core::mem::transmute::<unsafe extern "system" fn(), AntiLagUpdateFn>(f)
                },
                device: raw.handle(),
            })
        } else {
            None
        };
        let backend = if nv.is_some() {
            Backend::NvidiaReflex
        } else if amd.is_some() {
            Backend::AmdAntiLag
        } else {
            Backend::Generic
        };
        let mut latency = Self {
            backend,
            settings,
            device: raw.clone(),
            present_id: ext.present_id,
            present_wait: ext.present_id && ext.present_wait,
            nv,
            amd,
            external: None,
            // Present ids continue from whoever presented to this swapchain before.
            frame: swapchain.last_present_id(),
            next_frame_at: None,
            sleeper: Sleeper::new(),
            history: Box::new([FrameTimes::default(); HISTORY]),
        };
        latency.apply(swapchain)?;
        Ok(latency)
    }

    /// Latency control by `external` (see [`ExternalLatency`]): every call goes to it, and
    /// none to the driver's extensions. Frame ids continue from `swapchain`'s presents.
    pub fn with_external(
        device: &Device,
        swapchain: &impl LatencyTarget,
        settings: LatencySettings,
        external: Box<dyn ExternalLatency>,
    ) -> Result<Self, RhiError> {
        let ext = device.latency_extensions();
        let mut latency = Self {
            backend: Backend::External,
            settings,
            device: device.raw().clone(),
            present_id: ext.present_id,
            present_wait: false,
            nv: None,
            amd: None,
            external: Some(external),
            frame: swapchain.last_present_id(),
            next_frame_at: None,
            sleeper: Sleeper::new(),
            history: Box::new([FrameTimes::default(); HISTORY]),
        };
        latency.apply(swapchain)?;
        Ok(latency)
    }

    /// The backend in use.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// The current settings.
    pub fn settings(&self) -> LatencySettings {
        self.settings
    }

    /// Changes the settings.
    pub fn set_settings(
        &mut self,
        swapchain: &impl LatencyTarget,
        settings: LatencySettings,
    ) -> Result<(), RhiError> {
        self.settings = settings;
        self.next_frame_at = None;
        self.apply(swapchain)
    }

    /// Re-applies the settings to the driver. Call after the swapchain is recreated:
    /// Reflex settings belong to a swapchain.
    pub fn apply(&mut self, swapchain: &impl LatencyTarget) -> Result<(), RhiError> {
        if let Some(external) = &mut self.external {
            external.apply(&self.settings);
            return Ok(());
        }
        if let Some(nv) = &self.nv {
            let interval = self
                .settings
                .fps_limit
                .map_or(0, |fps| (1e6 / fps.max(1.0)) as u32);
            let info = vk::LatencySleepModeInfoNV::default()
                .low_latency_mode(self.settings.mode != LowLatency::Off)
                .low_latency_boost(self.settings.mode == LowLatency::Boost)
                .minimum_interval_us(interval);
            // SAFETY: the swapchain was created with latency mode enabled.
            unsafe {
                nv.loader
                    .set_latency_sleep_mode(swapchain.raw(), Some(&info))
            }
            .map_err(RhiError::vk("vkSetLatencySleepModeNV"))?;
        }
        if self.settings.mode == LowLatency::Off {
            // Anti-Lag 2 keeps its last mode: switch it off explicitly.
            self.anti_lag(ANTI_LAG_STAGE_INPUT);
        }
        Ok(())
    }

    fn times(&mut self) -> &mut FrameTimes {
        &mut self.history[(self.frame % HISTORY as u64) as usize]
    }

    fn anti_lag(&self, stage: i32) {
        if let Some(amd) = &self.amd {
            amd.update(
                self.settings.mode != LowLatency::Off,
                self.max_fps(),
                stage,
                self.frame,
            );
        }
    }

    fn max_fps(&self) -> u32 {
        self.settings.fps_limit.map_or(0, |f| f.round() as u32)
    }

    /// The calls a [`PacedPresenter`]'s thread makes around real frames' presents, for
    /// `swapchain`; `None` with no latency backend on or with the generic path. Pass them
    /// again ([`PacedPresenter::set_markers`]) after changing the settings.
    pub fn present_markers(&self, swapchain: &impl LatencyTarget) -> Option<PresentMarkers> {
        if self.settings.mode == LowLatency::Off || self.external.is_some() {
            return None;
        }
        let nv = self
            .nv
            .as_ref()
            .map(|nv| (nv.loader.clone(), swapchain.raw()));
        (nv.is_some() || self.amd.is_some()).then(|| PresentMarkers {
            nv,
            amd: self.amd,
            max_fps: self.max_fps(),
        })
    }

    /// Starts the next frame and returns its present id (one more than the swapchain's
    /// last): sleeps as the backend decides, so input sampled right after is as fresh as
    /// possible.
    ///
    /// `frames` bounds the GPU queue on the generic path when the device has no
    /// `VK_KHR_present_wait`.
    pub fn begin_frame(
        &mut self,
        swapchain: &impl LatencyTarget,
        frames: &Frames,
    ) -> Result<u64, RhiError> {
        engine_profiler::zone!("latency::begin_frame");
        self.frame += 1;
        let on = self.settings.mode != LowLatency::Off;
        if let Some(external) = &mut self.external {
            // The library sleeps and caps the frame rate itself, in every mode.
            external.sleep(self.frame);
            let frame = self.frame;
            *self.times() = FrameTimes {
                frame,
                begin: Some(Instant::now()),
                marks: [None; MARKERS],
            };
            return Ok(frame);
        }
        let vendor = match (&mut self.nv, on) {
            (Some(nv), true) => {
                nv.value += 1;
                let info = vk::LatencySleepInfoNV::default()
                    .signal_semaphore(nv.semaphore)
                    .value(nv.value);
                // SAFETY: the swapchain was created with latency mode enabled; the
                // semaphore is our timeline semaphore.
                unsafe { nv.loader.latency_sleep(swapchain.raw(), &info) }
                    .map_err(RhiError::vk("vkLatencySleepNV"))?;
                let semaphores = [nv.semaphore];
                let values = [nv.value];
                // SAFETY: waiting on our own timeline semaphore; a timeout only means
                // the driver's signal is late (the next sleep's value implies this one's).
                match unsafe {
                    self.device.wait_semaphores(
                        &vk::SemaphoreWaitInfo::default()
                            .semaphores(&semaphores)
                            .values(&values),
                        sleep_bound_ns(nv.interval_ns),
                    )
                } {
                    Ok(()) => {}
                    Err(vk::Result::TIMEOUT) => nv.cut += 1,
                    Err(e) => {
                        return Err(RhiError::Vulkan {
                            call: "vkWaitSemaphores(latency)",
                            result: e,
                        });
                    }
                }
                let now = Instant::now();
                if let Some(woke) = nv.woke {
                    nv.interval_ns = (now - woke).as_nanos() as u64;
                }
                nv.woke = Some(now);
                true
            }
            _ => false,
        };
        if !vendor && self.amd.is_some() && on {
            // Anti-Lag 2 sleeps inside the input-stage call and may cap the frame rate,
            // but its cap is not applied everywhere (ref-1's Radeon iGPU ran 640 FPS with
            // a cap of 233): cap on the CPU as well. When the driver did cap, the frame is
            // already due and this does not sleep.
            self.anti_lag(ANTI_LAG_STAGE_INPUT);
            self.limit_rate();
        } else if !vendor {
            // Generic: bound the present queue, then cap the frame rate.
            let depth = u64::from(self.settings.max_queued_presents);
            if on && depth > 0 {
                if self.present_wait && swapchain.can_wait_for_present() {
                    if self.frame > depth + 1 {
                        swapchain
                            .wait_for_present(self.frame - depth - 1, PRESENT_WAIT_TIMEOUT_NS)?;
                    }
                } else {
                    // No present_wait: bound the GPU queue instead. With `depth` frames
                    // allowed beyond this one, the frame before those must be finished.
                    let submitted = frames.submitted();
                    if submitted > depth {
                        frames.wait_for_frame(submitted - depth)?;
                    }
                }
            }
            self.limit_rate();
        }
        let frame = self.frame;
        let times = self.times();
        *times = FrameTimes {
            frame,
            begin: Some(Instant::now()),
            marks: [None; MARKERS],
        };
        Ok(frame)
    }

    /// The CPU frame-rate limiter: sleeps until the frame is due on a high-resolution
    /// waitable timer.
    fn limit_rate(&mut self) {
        if let Some(fps) = self.settings.fps_limit {
            let interval = Duration::from_secs_f64(1.0 / fps.max(1.0));
            let now = Instant::now();
            let at = self.next_frame_at.unwrap_or(now);
            if at > now {
                self.sleeper.sleep_until(at);
            }
            // Catch up at most one frame after a stall instead of bursting.
            self.next_frame_at = Some(at.max(now) + interval);
        }
    }

    /// Records a marker for the current frame.
    pub fn mark(&mut self, swapchain: &impl LatencyTarget, marker: Marker) {
        let now = Instant::now();
        self.times().marks[marker.index()] = Some(now);
        if let Some(external) = &mut self.external {
            external.mark(self.frame, marker);
        }
        if let Some(nv) = &self.nv
            && self.settings.mode != LowLatency::Off
        {
            let info = vk::SetLatencyMarkerInfoNV::default()
                .present_id(self.frame)
                .marker(marker.nv());
            // SAFETY: the swapchain was created with latency mode enabled.
            unsafe { nv.loader.set_latency_marker(swapchain.raw(), &info) };
        }
        if marker == Marker::PresentEnd
            && let Some(input) = self.times().at(Marker::InputSample)
        {
            engine_profiler::plot!(
                "latency::input_to_present_ms",
                (now - input).as_secs_f64() * 1e3
            );
        }
    }

    /// Call right before presenting (Anti-Lag 2's present stage).
    pub fn before_present(&mut self) {
        if self.settings.mode != LowLatency::Off {
            self.anti_lag(ANTI_LAG_STAGE_PRESENT);
        }
    }

    /// The id to present the current frame with, when present ids are enabled.
    pub fn present_id(&self) -> Option<u64> {
        self.present_id.then_some(self.frame)
    }

    /// How many Reflex sleeps were cut at their bound: three recent frame intervals
    /// and a millisecond (ADR 0023, addendum 2026-10-16). 0 on other backends.
    pub fn sleeps_cut(&self) -> u64 {
        self.nv.as_ref().map_or(0, |nv| nv.cut)
    }

    /// The id to tag the current frame's queue submissions with (Reflex only).
    pub fn submit_present_id(&self) -> Option<u64> {
        (self.nv.is_some() && self.settings.mode != LowLatency::Off).then_some(self.frame)
    }

    /// CPU marker times of recent frames (up to [`HISTORY`], unordered ring).
    pub fn history(&self) -> &[FrameTimes] {
        &self.history[..]
    }

    /// Reflex's own per-frame report (driver and GPU timestamps, µs on the driver's
    /// clock), newest last. Empty on other backends.
    pub fn nv_reports(
        &self,
        swapchain: &impl LatencyTarget,
        out: &mut Vec<NvFrameReport>,
    ) -> Result<(), RhiError> {
        out.clear();
        let Some(nv) = &self.nv else { return Ok(()) };
        let mut reports = [vk::LatencyTimingsFrameReportNV::default(); 64];
        let mut info = vk::GetLatencyMarkerInfoNV::default().timings(&mut reports);
        // SAFETY: the swapchain was created with latency mode enabled; the driver fills
        // up to 64 reports.
        unsafe { nv.loader.get_latency_timings(swapchain.raw(), &mut info) };
        for r in &reports {
            if r.present_id != 0 {
                out.push(NvFrameReport {
                    present_id: r.present_id,
                    input_sample_us: r.input_sample_time_us,
                    sim_start_us: r.sim_start_time_us,
                    sim_end_us: r.sim_end_time_us,
                    render_submit_start_us: r.render_submit_start_time_us,
                    render_submit_end_us: r.render_submit_end_time_us,
                    present_start_us: r.present_start_time_us,
                    present_end_us: r.present_end_time_us,
                    driver_start_us: r.driver_start_time_us,
                    driver_end_us: r.driver_end_time_us,
                    os_queue_start_us: r.os_render_queue_start_time_us,
                    os_queue_end_us: r.os_render_queue_end_time_us,
                    gpu_render_start_us: r.gpu_render_start_time_us,
                    gpu_render_end_us: r.gpu_render_end_time_us,
                });
            }
        }
        out.sort_by_key(|r| r.present_id);
        Ok(())
    }
}

/// One frame of Reflex's report (microseconds on the driver's clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvFrameReport {
    /// The frame's present id.
    pub present_id: u64,
    /// Input sampled (the `InputSample` marker).
    pub input_sample_us: u64,
    /// Simulation started.
    pub sim_start_us: u64,
    /// Simulation ended.
    pub sim_end_us: u64,
    /// Render submission started.
    pub render_submit_start_us: u64,
    /// Render submission ended.
    pub render_submit_end_us: u64,
    /// Present call started.
    pub present_start_us: u64,
    /// Present call returned.
    pub present_end_us: u64,
    /// The driver started processing the frame's submissions.
    pub driver_start_us: u64,
    /// The driver finished processing them.
    pub driver_end_us: u64,
    /// The frame entered the OS's render queue.
    pub os_queue_start_us: u64,
    /// The frame left the OS's render queue.
    pub os_queue_end_us: u64,
    /// GPU started rendering the frame.
    pub gpu_render_start_us: u64,
    /// GPU finished rendering the frame.
    pub gpu_render_end_us: u64,
}

impl NvFrameReport {
    /// Input sample to GPU completion, the part of input-to-photon the PC controls.
    ///
    /// NVIDIA's driver 616.92 puts the GPU times of the next frame's work in a frame's
    /// report when submissions carry their present id, as the extension asks
    /// (`VkLatencySubmissionPresentIdNV`): this then reads about one frame interval
    /// high (ADR 0023, addendum 2026-10-16).
    pub fn input_to_gpu_end_us(&self) -> Option<u64> {
        (self.input_sample_us > 0 && self.gpu_render_end_us > self.input_sample_us)
            .then(|| self.gpu_render_end_us - self.input_sample_us)
    }
}

impl Drop for Latency {
    fn drop(&mut self) {
        if let Some(nv) = &self.nv {
            // SAFETY: the device is idle for this semaphore once the owner stopped
            // rendering (the caller's frames waited); nothing else uses it.
            unsafe { self.device.destroy_semaphore(nv.semaphore, None) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sleep_is_cut_at_three_intervals_within_its_limits() {
        // Nothing measured yet: the longest wait.
        assert_eq!(sleep_bound_ns(0), NV_SLEEP_TIMEOUT_NS);
        // 2 ms frames: 7 ms.
        assert_eq!(sleep_bound_ns(2_000_000), 7_000_000);
        // Very short frames: not under 4 ms.
        assert_eq!(sleep_bound_ns(500_000), NV_SLEEP_MIN_BOUND_NS);
        // A 60 FPS cap: 51 ms.
        assert_eq!(sleep_bound_ns(16_666_667), 51_000_001);
        // A long pause: not over 200 ms.
        assert_eq!(sleep_bound_ns(5_000_000_000), NV_SLEEP_TIMEOUT_NS);
    }
}
