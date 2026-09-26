//! WASAPI output: a dedicated audio thread that feeds the default render device.
//!
//! - Shared mode with `IAudioClient3` for periods below the 10 ms default (the device
//!   period is the requested one rounded to the engine's fundamental period), falling back
//!   to `IAudioClient::Initialize` on drivers or endpoints that refuse it.
//! - Event-driven: the thread waits on the device's event, the stop event and a
//!   device-change event, and renders exactly the free space of the device buffer.
//! - MMCSS "Pro Audio", performance cores, stream category `GameEffects`.
//! - Follows the default device (headphones plugged in); without any device it keeps
//!   rendering on a timer at the last rate, so voices still end on time.
//!
//! The render path allocates nothing; opening a device does.

mod com;
mod notify;

use crate::mixer::Renderer;
use com::{
    AudioClientProperties, Com, IAudioClient3Vtbl, IAudioClientVtbl, IAudioRenderClientVtbl,
    IID_IAUDIO_CLIENT, IID_IAUDIO_CLIENT3, IID_IAUDIO_RENDER_CLIENT, IID_IMM_DEVICE_ENUMERATOR,
    IMMDeviceEnumeratorVtbl, IMMDeviceVtbl, IMMNotificationClientVtbl, IPropertyStoreVtbl,
    PKEY_DEVICE_FRIENDLY_NAME, PropVariant, VT_LPWSTR, check, same_guid, wide_to_string,
};
use core::ffi::c_void;
use engine_core::spsc;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AudioCategory_GameEffects, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows_sys::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows_sys::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::core::GUID;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// 100-ns units per second (`REFERENCE_TIME`).
const REFTIMES_PER_SEC: f64 = 1e7;

/// How to open the output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputConfig {
    /// Requested device period in milliseconds (CLAUDE.md §11 budget: 5 ms). Rounded to
    /// what the driver supports; `0.0` asks for the driver's minimum.
    pub period_ms: f32,
    /// Try `IAudioClient3` small periods; `false` always uses the standard ~10 ms period.
    pub low_latency: bool,
    /// Audio kept queued in the device buffer: one period plus this many milliseconds.
    /// The Windows audio engine takes one period per event, so a small margin suffices
    /// and every millisecond here is a millisecond of output latency. `None` fills the
    /// whole device buffer (usually over two periods): the most robust, and the highest
    /// latency.
    pub queue_margin_ms: Option<f32>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            period_ms: 5.0,
            low_latency: true,
            queue_margin_ms: Some(2.0),
        }
    }
}

/// How the output currently runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// `IAudioClient3` shared stream with a small period.
    LowLatency,
    /// `IAudioClient::Initialize` shared stream (the device's default period).
    Standard,
    /// No render device: the mixer runs on a timer and its output is discarded.
    NoDevice,
}

/// Sample format of the device buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// 32-bit float (the usual shared-mode format; rendered in place).
    F32,
    /// 16-bit integer.
    I16,
    /// 24-bit integer in 3 bytes.
    I24,
    /// 32-bit integer (also 24 valid bits in 32).
    I32,
}

/// The audio engine's shared-mode periods for the device format, in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnginePeriods {
    /// Default period (usually 10 ms).
    pub default: u32,
    /// Periods must be multiples of this.
    pub fundamental: u32,
    /// Smallest period.
    pub min: u32,
    /// Largest period.
    pub max: u32,
}

/// The device the output is using.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// How the stream was opened.
    pub mode: OutputMode,
    /// Device friendly name.
    pub device: String,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Device channels (the mix is stereo; extra channels get silence).
    pub channels: u16,
    /// Buffer sample format.
    pub format: SampleFormat,
    /// Period between device events, in frames.
    pub period_frames: u32,
    /// Device buffer size in frames (the most audio that can be queued).
    pub buffer_frames: u32,
    /// Frames the output keeps queued after each write (one period plus the margin, or
    /// the whole buffer): the output latency before the Windows engine and the driver.
    pub queue_frames: u32,
    /// `IAudioClient::GetStreamLatency`, in frames.
    pub stream_latency_frames: u32,
    /// Engine periods when `IAudioClient3` was available.
    pub engine_periods: Option<EnginePeriods>,
    /// Why low latency or the device could not be used, if it could not.
    pub note: Option<String>,
}

impl OutputInfo {
    fn no_device(sample_rate: u32, note: String) -> Self {
        Self {
            mode: OutputMode::NoDevice,
            device: String::new(),
            sample_rate,
            channels: 2,
            format: SampleFormat::F32,
            period_frames: sample_rate / 100,
            buffer_frames: 0,
            queue_frames: 0,
            stream_latency_frames: 0,
            engine_periods: None,
            note: Some(note),
        }
    }
}

const BUCKETS: usize = 128;

/// A fixed-bucket histogram of microsecond durations, filled lock-free.
struct Histogram {
    bucket_us: u32,
    counts: [AtomicU32; BUCKETS],
    max_us: AtomicU32,
}

impl Histogram {
    fn new(bucket_us: u32) -> Self {
        Self {
            bucket_us,
            counts: core::array::from_fn(|_| AtomicU32::new(0)),
            max_us: AtomicU32::new(0),
        }
    }

    fn record(&self, d: Duration) {
        let us = d.as_micros().min(u128::from(u32::MAX)) as u32;
        let bucket = ((us / self.bucket_us) as usize).min(BUCKETS - 1);
        self.counts[bucket].fetch_add(1, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Durations {
        Durations {
            bucket_us: self.bucket_us,
            counts: core::array::from_fn(|i| self.counts[i].load(Ordering::Relaxed)),
            max_us: self.max_us.load(Ordering::Relaxed),
        }
    }
}

/// A snapshot of a duration histogram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Durations {
    bucket_us: u32,
    counts: [u32; BUCKETS],
    max_us: u32,
}

impl Durations {
    /// Samples recorded.
    pub fn count(&self) -> u64 {
        self.counts.iter().map(|&c| u64::from(c)).sum()
    }

    /// The `p`-th percentile (0..=1) in microseconds, to the bucket's upper edge.
    pub fn percentile_us(&self, p: f64) -> u32 {
        let total = self.count();
        if total == 0 {
            return 0;
        }
        let rank = ((total as f64 * p.clamp(0.0, 1.0)).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += u64::from(c);
            if seen >= rank {
                return if i == BUCKETS - 1 {
                    self.max_us
                } else {
                    (i as u32 + 1) * self.bucket_us
                };
            }
        }
        self.max_us
    }

    /// Largest sample in microseconds.
    pub fn max_us(&self) -> u32 {
        self.max_us
    }
}

struct Stats {
    callbacks: AtomicU64,
    empty_wakes: AtomicU64,
    late_wakes: AtomicU64,
    reopens: AtomicU32,
    frames: AtomicU64,
    padding_frames: AtomicU64,
    interval: Histogram,
    render: Histogram,
}

/// Counters of the output thread since it started.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputStats {
    /// Device events handled.
    pub callbacks: u64,
    /// Wake-ups that found the device buffer empty. When the buffer is larger than one
    /// period this is an audible gap; when it is exactly one period it is normal.
    pub empty_wakes: u64,
    /// Wake-ups more than 1.5 periods after the previous one.
    pub late_wakes: u64,
    /// Times the stream was reopened (default device changed, device lost).
    pub reopens: u32,
    /// Frames rendered to the device.
    pub frames: u64,
    /// Average frames still queued in the device buffer when the thread woke (the safety
    /// margin left; the output latency is roughly this plus what was just written).
    pub average_padding_frames: f64,
    /// Time between device events.
    pub interval: Durations,
    /// Time to render and write one event's frames.
    pub render: Durations,
}

/// A Win32 event handle.
struct Event(HANDLE);

// SAFETY: event handles can be waited on and signaled from any thread.
unsafe impl Send for Event {}
// SAFETY: as above; `SetEvent` and waits are thread-safe.
unsafe impl Sync for Event {}

impl Event {
    fn new() -> io::Result<Self> {
        // SAFETY: an unnamed auto-reset event with default security.
        let h = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        if h.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(h))
        }
    }

    fn set(&self) {
        // SAFETY: the handle is open while `self` lives.
        unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it once.
        unsafe { CloseHandle(self.0) };
    }
}

struct Control {
    stop: Event,
    ready: Event,
    stats: Stats,
}

/// The running output. Dropping it stops the audio thread.
pub struct Output {
    thread: Option<JoinHandle<Renderer>>,
    control: Arc<Control>,
    infos: spsc::Consumer<OutputInfo>,
    info: OutputInfo,
}

impl Output {
    /// Starts the audio thread with `renderer` on the default render device. Returns once
    /// the first device is open (or known to be missing), so [`Output::info`] and the
    /// renderer's sample rate are already the device's.
    pub fn start(renderer: Renderer, config: OutputConfig) -> io::Result<Self> {
        let control = Arc::new(Control {
            stop: Event::new()?,
            ready: Event::new()?,
            stats: Stats {
                callbacks: AtomicU64::new(0),
                empty_wakes: AtomicU64::new(0),
                late_wakes: AtomicU64::new(0),
                reopens: AtomicU32::new(0),
                frames: AtomicU64::new(0),
                padding_frames: AtomicU64::new(0),
                interval: Histogram::new(100),
                render: Histogram::new(5),
            },
        });
        let (info_tx, infos) = spsc::channel(16);
        let thread_control = control.clone();
        let thread = std::thread::Builder::new()
            .name(String::from("engine-audio"))
            .spawn(move || run(renderer, config, &thread_control, info_tx))?;
        // SAFETY: waiting on our own event; 5 s covers slow driver initialization.
        unsafe { WaitForSingleObject(control.ready.0, 5_000) };
        let rate = 48_000;
        let mut output = Self {
            thread: Some(thread),
            control,
            infos,
            info: OutputInfo::no_device(rate, String::from("starting")),
        };
        output.poll();
        Ok(output)
    }

    fn poll(&mut self) {
        while let Some(info) = self.infos.pop() {
            self.info = info;
        }
    }

    /// The device in use (updated when the device changes).
    pub fn info(&mut self) -> &OutputInfo {
        self.poll();
        &self.info
    }

    /// Counters since the start.
    pub fn stats(&self) -> OutputStats {
        let s = &self.control.stats;
        let callbacks = s.callbacks.load(Ordering::Relaxed);
        OutputStats {
            callbacks,
            empty_wakes: s.empty_wakes.load(Ordering::Relaxed),
            late_wakes: s.late_wakes.load(Ordering::Relaxed),
            reopens: s.reopens.load(Ordering::Relaxed),
            frames: s.frames.load(Ordering::Relaxed),
            average_padding_frames: s.padding_frames.load(Ordering::Relaxed) as f64
                / callbacks.max(1) as f64,
            interval: s.interval.snapshot(),
            render: s.render.snapshot(),
        }
    }

    /// Stops the audio thread and returns the renderer.
    pub fn stop(mut self) -> Option<Renderer> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Option<Renderer> {
        self.control.stop.set();
        self.thread.take().and_then(|t| t.join().ok())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone, Copy)]
struct Format {
    sample_rate: u32,
    channels: u16,
    sample: SampleFormat,
    block_align: u16,
}

/// Reads a `WAVEFORMATEX` (or `WAVEFORMATEXTENSIBLE`) from the system.
///
/// # Safety
/// `p` points to a valid format structure.
unsafe fn read_format(p: *const c_void) -> Option<Format> {
    // SAFETY: the caller guarantees a valid structure; it is packed, so read unaligned.
    let wf = unsafe { core::ptr::read_unaligned(p.cast::<WAVEFORMATEX>()) };
    let (tag, bits, cb) = (wf.wFormatTag, wf.wBitsPerSample, wf.cbSize);
    let tag = if tag == WAVE_FORMAT_EXTENSIBLE && cb >= 22 {
        // SAFETY: an extensible format has its sub-format GUID at offset 24.
        let sub = unsafe { core::ptr::read_unaligned(p.cast::<u8>().add(24).cast::<GUID>()) };
        if same_guid(&sub, &SUBTYPE_IEEE_FLOAT) {
            WAVE_FORMAT_IEEE_FLOAT
        } else if same_guid(&sub, &SUBTYPE_PCM) {
            WAVE_FORMAT_PCM
        } else {
            return None;
        }
    } else {
        tag
    };
    let sample = match (tag, bits) {
        (WAVE_FORMAT_IEEE_FLOAT, 32) => SampleFormat::F32,
        (WAVE_FORMAT_PCM, 16) => SampleFormat::I16,
        (WAVE_FORMAT_PCM, 24) => SampleFormat::I24,
        (WAVE_FORMAT_PCM, 32) => SampleFormat::I32,
        _ => return None,
    };
    Some(Format {
        sample_rate: wf.nSamplesPerSec,
        channels: wf.nChannels,
        sample,
        block_align: wf.nBlockAlign,
    })
}

/// A format structure allocated by the system, freed on drop.
struct CoFormat(*mut c_void);

impl Drop for CoFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by WASAPI with the COM allocator; freed once.
        unsafe { CoTaskMemFree(self.0) };
    }
}

struct Stream {
    client: Com<IAudioClientVtbl>,
    render: Com<IAudioRenderClientVtbl>,
    // Signaled by the device every period; must outlive the client.
    event: Event,
    format: Format,
    buffer_frames: u32,
    device_id: String,
    info: OutputInfo,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stopping a started or initialized client is allowed; errors are moot.
        unsafe { (self.client.vtbl().Stop)(self.client.raw()) };
    }
}

fn hr_text(what: &str, hr: i32) -> String {
    format!("{what} failed: HRESULT {:#010x}", hr as u32)
}

/// The current default render endpoint and its id.
fn default_device(
    enumerator: &Com<IMMDeviceEnumeratorVtbl>,
) -> Result<(Com<IMMDeviceVtbl>, String), String> {
    let mut raw = core::ptr::null_mut();
    // SAFETY: valid enumerator and out pointer.
    check(unsafe {
        (enumerator.vtbl().GetDefaultAudioEndpoint)(enumerator.raw(), eRender, eConsole, &mut raw)
    })
    .map_err(|hr| hr_text("GetDefaultAudioEndpoint", hr))?;
    // SAFETY: an IMMDevice pointer we now own.
    let device = unsafe { Com::<IMMDeviceVtbl>::from_raw(raw) }.ok_or("no default device")?;
    let mut id = core::ptr::null_mut();
    // SAFETY: valid device and out pointer; the string is freed below.
    let id = if check(unsafe { (device.vtbl().GetId)(device.raw(), &mut id) }).is_ok() {
        // SAFETY: a NUL-terminated string from the system, freed once after reading.
        unsafe {
            let s = wide_to_string(id);
            CoTaskMemFree(id.cast());
            s
        }
    } else {
        String::new()
    };
    Ok((device, id))
}

fn friendly_name(device: &Com<IMMDeviceVtbl>) -> String {
    let mut raw = core::ptr::null_mut();
    // SAFETY: STGM_READ (0) property store of a live device.
    if check(unsafe { (device.vtbl().OpenPropertyStore)(device.raw(), 0, &mut raw) }).is_err() {
        return String::new();
    }
    // SAFETY: an IPropertyStore pointer we now own.
    let Some(store) = (unsafe { Com::<IPropertyStoreVtbl>::from_raw(raw) }) else {
        return String::new();
    };
    let mut value = PropVariant {
        vt: 0,
        reserved: [0; 3],
        data: [0; 2],
    };
    // SAFETY: valid key and out value; the value is cleared below.
    let ok = check(unsafe {
        (store.vtbl().GetValue)(store.raw(), &PKEY_DEVICE_FRIENDLY_NAME, &mut value)
    })
    .is_ok();
    let name = if ok && value.vt == VT_LPWSTR {
        // SAFETY: VT_LPWSTR holds a NUL-terminated string in the first union word.
        unsafe { wide_to_string(value.data[0] as *const u16) }
    } else {
        String::new()
    };
    // SAFETY: clearing a PROPVARIANT we received (its layout matches the system's).
    unsafe { PropVariantClear((&raw mut value).cast()) };
    name
}

fn open(
    enumerator: &Com<IMMDeviceEnumeratorVtbl>,
    config: &OutputConfig,
) -> Result<Stream, String> {
    let (device, device_id) = default_device(enumerator)?;
    let name = friendly_name(&device);
    let mut note = None;
    if config.low_latency {
        match open_client(&device, config, true) {
            Ok(mut stream) => {
                stream.device_id = device_id;
                stream.info.device = name;
                return Ok(stream);
            }
            Err(e) => note = Some(format!("low latency unavailable: {e}")),
        }
    }
    let mut stream = open_client(&device, config, false)?;
    stream.device_id = device_id;
    stream.info.device = name;
    stream.info.note = note;
    Ok(stream)
}

fn open_client(
    device: &Com<IMMDeviceVtbl>,
    config: &OutputConfig,
    low_latency: bool,
) -> Result<Stream, String> {
    let iid = if low_latency {
        IID_IAUDIO_CLIENT3
    } else {
        IID_IAUDIO_CLIENT
    };
    let mut raw = core::ptr::null_mut();
    // SAFETY: valid device, IID and out pointer.
    check(unsafe {
        (device.vtbl().Activate)(device.raw(), &iid, CLSCTX_ALL, core::ptr::null(), &mut raw)
    })
    .map_err(|hr| hr_text("IMMDevice::Activate", hr))?;
    // SAFETY: an audio client pointer we now own (IAudioClient3 derives from IAudioClient).
    let client = unsafe { Com::<IAudioClientVtbl>::from_raw(raw) }.ok_or("null audio client")?;
    let c = client.vtbl();
    // SAFETY: activated for IID_IAudioClient3 when `low_latency`, so the vtable has the
    // IAudioClient2/3 methods; not used otherwise.
    let v3 = low_latency.then(|| unsafe { client.cast::<IAudioClient3Vtbl>() });

    if let Some(v3) = v3 {
        let props = AudioClientProperties {
            cbSize: size_of::<AudioClientProperties>() as u32,
            bIsOffload: 0,
            eCategory: AudioCategory_GameEffects,
            Options: 0,
        };
        // SAFETY: valid properties; failure only loses the category.
        unsafe { (v3.vtbl().SetClientProperties)(v3.raw(), &props) };
    }

    let mut fmt = core::ptr::null_mut();
    // SAFETY: valid client and out pointer; freed by `CoFormat`.
    check(unsafe { (c.GetMixFormat)(client.raw(), &mut fmt) })
        .map_err(|hr| hr_text("GetMixFormat", hr))?;
    let fmt = CoFormat(fmt);
    // SAFETY: WASAPI returned a valid format structure.
    let format = unsafe { read_format(fmt.0) }.ok_or("unsupported mix format")?;
    let event = Event::new().map_err(|e| e.to_string())?;
    let rate = format.sample_rate;

    let mut engine_periods = None;
    let period_frames;
    let mode;
    if let Some(v3) = v3 {
        let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
        // SAFETY: valid format and out pointers.
        check(unsafe {
            (v3.vtbl().GetSharedModeEnginePeriod)(
                v3.raw(),
                fmt.0,
                &mut default,
                &mut fundamental,
                &mut min,
                &mut max,
            )
        })
        .map_err(|hr| hr_text("GetSharedModeEnginePeriod", hr))?;
        engine_periods = Some(EnginePeriods {
            default,
            fundamental,
            min,
            max,
        });
        let fundamental = fundamental.max(1);
        let desired = (config.period_ms.max(0.0) * rate as f32 / 1000.0) as u32;
        // The nearest multiple of the fundamental period within [min, max].
        let below = desired / fundamental * fundamental;
        let nearest = if desired - below > fundamental / 2 {
            below + fundamental
        } else {
            below
        };
        let period = nearest.clamp(min.max(fundamental), max.max(min));
        // SAFETY: valid format; the event-callback flag is supported in shared mode.
        check(unsafe {
            (v3.vtbl().InitializeSharedAudioStream)(
                v3.raw(),
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period,
                fmt.0,
                core::ptr::null(),
            )
        })
        .map_err(|hr| hr_text("InitializeSharedAudioStream", hr))?;
        let mut current_fmt = core::ptr::null_mut();
        let mut current = period;
        // SAFETY: valid out pointers; the returned format is freed right away.
        if check(unsafe {
            (v3.vtbl().GetCurrentSharedModeEnginePeriod)(v3.raw(), &mut current_fmt, &mut current)
        })
        .is_ok()
        {
            drop(CoFormat(current_fmt));
        }
        period_frames = current;
        mode = OutputMode::LowLatency;
    } else {
        // SAFETY: valid format; a zero buffer duration asks for the smallest buffer.
        check(unsafe {
            (c.Initialize)(
                client.raw(),
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                fmt.0,
                core::ptr::null(),
            )
        })
        .map_err(|hr| hr_text("IAudioClient::Initialize", hr))?;
        let (mut default, mut min) = (0i64, 0i64);
        // SAFETY: valid out pointers.
        unsafe { (c.GetDevicePeriod)(client.raw(), &mut default, &mut min) };
        period_frames = (default as f64 / REFTIMES_PER_SEC * f64::from(rate)).round() as u32;
        mode = OutputMode::Standard;
    }

    // SAFETY: the client is initialized for event callbacks; the event outlives it
    // (`Stream` drops the client first).
    check(unsafe { (c.SetEventHandle)(client.raw(), event.0) })
        .map_err(|hr| hr_text("SetEventHandle", hr))?;
    let mut buffer_frames = 0;
    // SAFETY: valid out pointer.
    check(unsafe { (c.GetBufferSize)(client.raw(), &mut buffer_frames) })
        .map_err(|hr| hr_text("GetBufferSize", hr))?;
    let mut raw = core::ptr::null_mut();
    // SAFETY: valid IID and out pointer.
    check(unsafe { (c.GetService)(client.raw(), &IID_IAUDIO_RENDER_CLIENT, &mut raw) })
        .map_err(|hr| hr_text("GetService(IAudioRenderClient)", hr))?;
    // SAFETY: an IAudioRenderClient pointer we now own.
    let render =
        unsafe { Com::<IAudioRenderClientVtbl>::from_raw(raw) }.ok_or("null render client")?;
    let mut latency = 0i64;
    // SAFETY: valid out pointer.
    unsafe { (c.GetStreamLatency)(client.raw(), &mut latency) };

    Ok(Stream {
        info: OutputInfo {
            mode,
            device: String::new(),
            sample_rate: rate,
            channels: format.channels,
            format: format.sample,
            period_frames,
            buffer_frames,
            queue_frames: match config.queue_margin_ms {
                Some(ms) => (period_frames + (ms.max(0.0) * rate as f32 / 1000.0) as u32)
                    .clamp(1, buffer_frames),
                None => buffer_frames,
            },
            stream_latency_frames: (latency as f64 / REFTIMES_PER_SEC * f64::from(rate)).round()
                as u32,
            engine_periods,
            note: None,
        },
        client,
        render,
        event,
        format,
        buffer_frames,
        device_id: String::new(),
    })
}

/// Converts interleaved `f32` to the device's integer format.
fn convert(src: &[f32], dst: *mut u8, format: SampleFormat) {
    let clamp = |x: f32| x.clamp(-1.0, 1.0);
    match format {
        SampleFormat::F32 => {
            for (i, &x) in src.iter().enumerate() {
                // SAFETY: the caller's buffer holds `src.len()` samples of this format.
                unsafe { dst.cast::<f32>().add(i).write_unaligned(x) };
            }
        }
        SampleFormat::I16 => {
            for (i, &x) in src.iter().enumerate() {
                // SAFETY: as above.
                unsafe {
                    dst.cast::<i16>()
                        .add(i)
                        .write_unaligned((clamp(x) * 32_767.0) as i16);
                }
            }
        }
        SampleFormat::I24 => {
            for (i, &x) in src.iter().enumerate() {
                let v = ((clamp(x) * 8_388_607.0) as i32).to_le_bytes();
                // SAFETY: as above; 3 bytes per sample.
                unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), dst.add(i * 3), 3) };
            }
        }
        SampleFormat::I32 => {
            for (i, &x) in src.iter().enumerate() {
                // SAFETY: as above.
                unsafe {
                    dst.cast::<i32>()
                        .add(i)
                        .write_unaligned((f64::from(clamp(x)) * 2_147_483_647.0) as i32);
                }
            }
        }
    }
}

enum Exit {
    Stop,
    /// The stream's device changed or went away.
    Reopen,
    /// No device yet; try again.
    Retry,
}

fn run(
    mut renderer: Renderer,
    config: OutputConfig,
    control: &Control,
    mut infos: spsc::Producer<OutputInfo>,
) -> Renderer {
    use engine_platform::thread::{MmcssRegistration, MmcssTask};
    let _ = engine_platform::thread::set_current_thread_description("engine audio");
    if let Ok(topology) = engine_platform::cpu::CpuTopology::query() {
        let _ =
            engine_platform::thread::set_current_thread_cpu_sets(&topology.performance_set_ids());
    }
    let _mmcss = MmcssRegistration::join(MmcssTask::ProAudio).ok();
    engine_profiler::set_thread_name("audio");

    // SAFETY: COM for this thread, balanced by `CoUninitialize` below.
    let com = unsafe { CoInitializeEx(core::ptr::null(), COINIT_MULTITHREADED as u32) };
    let mut raw = core::ptr::null_mut();
    // SAFETY: the system device enumerator.
    let created = check(unsafe {
        CoCreateInstance(
            &MMDeviceEnumerator,
            core::ptr::null_mut(),
            CLSCTX_ALL,
            &IID_IMM_DEVICE_ENUMERATOR,
            &mut raw,
        )
    });
    // SAFETY: an IMMDeviceEnumerator pointer we now own (null on failure).
    let enumerator = created
        .ok()
        .and_then(|()| unsafe { Com::<IMMDeviceEnumeratorVtbl>::from_raw(raw) });
    let notifier: Option<Com<IMMNotificationClientVtbl>> = notify::create().ok();
    if let (Some(e), Some(n)) = (&enumerator, &notifier) {
        // SAFETY: registering our notifier; unregistered before it is released.
        unsafe { (e.vtbl().RegisterEndpointNotificationCallback)(e.raw(), n.raw()) };
    }
    let changed = notifier.as_ref().map(notify::event);

    let mut scratch: Vec<f32> = Vec::new();
    let mut first = true;
    loop {
        let opened = match &enumerator {
            Some(e) => open(e, &config),
            None => Err(String::from("no device enumerator")),
        };
        let exit = match opened {
            Ok(stream) => {
                renderer.set_sample_rate(stream.format.sample_rate);
                let needed = stream.buffer_frames as usize * usize::from(stream.format.channels);
                if scratch.len() < needed {
                    scratch.resize(needed, 0.0);
                }
                let _ = infos.push(stream.info.clone());
                if core::mem::take(&mut first) {
                    control.ready.set();
                }
                stream_loop(
                    &stream,
                    &mut renderer,
                    control,
                    changed,
                    enumerator.as_ref(),
                    &mut scratch,
                )
            }
            Err(note) => {
                let _ = infos.push(OutputInfo::no_device(renderer.sample_rate(), note));
                if core::mem::take(&mut first) {
                    control.ready.set();
                }
                silent_loop(&mut renderer, control, changed, &mut scratch)
            }
        };
        match exit {
            Exit::Stop => break,
            Exit::Reopen => {
                control.stats.reopens.fetch_add(1, Ordering::Relaxed);
            }
            Exit::Retry => {}
        }
    }

    if let (Some(e), Some(n)) = (&enumerator, &notifier) {
        // SAFETY: unregistering the notifier registered above.
        unsafe { (e.vtbl().UnregisterEndpointNotificationCallback)(e.raw(), n.raw()) };
    }
    drop(notifier);
    drop(enumerator);
    if com >= 0 {
        // SAFETY: balances the successful `CoInitializeEx`.
        unsafe { CoUninitialize() };
    }
    renderer
}

/// Waits on `handles` (stop first); returns the signaled index or `None` on timeout.
fn wait(handles: &[HANDLE], timeout_ms: u32) -> Option<usize> {
    // SAFETY: all handles are open events for the duration of the call.
    let r =
        unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout_ms) };
    if r == WAIT_TIMEOUT {
        None
    } else {
        Some(r.wrapping_sub(WAIT_OBJECT_0) as usize)
    }
}

fn stream_loop(
    stream: &Stream,
    renderer: &mut Renderer,
    control: &Control,
    changed: Option<HANDLE>,
    enumerator: Option<&Com<IMMDeviceEnumeratorVtbl>>,
    scratch: &mut [f32],
) -> Exit {
    let c = stream.client.vtbl();
    let r = stream.render.vtbl();
    let channels = usize::from(stream.format.channels);
    let stats = &control.stats;
    let direct = stream.format.sample == SampleFormat::F32
        && usize::from(stream.format.block_align) == channels * 4;

    // Fill one render into the free space; `Err` means the device is gone.
    let mut fill = |renderer: &mut Renderer| -> Result<u32, i32> {
        let mut padding = 0;
        // SAFETY: valid client and out pointer.
        check(unsafe { (c.GetCurrentPadding)(stream.client.raw(), &mut padding) })?;
        let frames = stream
            .info
            .queue_frames
            .min(stream.buffer_frames)
            .saturating_sub(padding);
        if frames == 0 {
            return Ok(padding);
        }
        let mut data = core::ptr::null_mut();
        // SAFETY: `frames` fit in the free space just measured.
        check(unsafe { (r.GetBuffer)(stream.render.raw(), frames, &mut data) })?;
        let samples = frames as usize * channels;
        if direct && (data as usize).is_multiple_of(4) {
            // SAFETY: WASAPI gave us `frames` frames of interleaved f32 at `data`, aligned
            // (checked), exclusively ours until `ReleaseBuffer`.
            let out = unsafe { core::slice::from_raw_parts_mut(data.cast::<f32>(), samples) };
            renderer.render(out, channels);
        } else {
            let buf = &mut scratch[..samples];
            renderer.render(buf, channels);
            convert(buf, data, stream.format.sample);
        }
        // SAFETY: releasing exactly the frames we got.
        check(unsafe { (r.ReleaseBuffer)(stream.render.raw(), frames, 0) })?;
        stats.frames.fetch_add(u64::from(frames), Ordering::Relaxed);
        Ok(padding)
    };

    // Prime the buffer with real audio so playback does not start with a silent period.
    if fill(renderer).is_err() {
        return Exit::Reopen;
    }
    // SAFETY: an initialized client.
    if check(unsafe { (c.Start)(stream.client.raw()) }).is_err() {
        return Exit::Reopen;
    }

    let mut handles = vec![control.stop.0, stream.event.0];
    if let Some(h) = changed {
        handles.push(h);
    }
    let period = Duration::from_secs_f64(
        f64::from(stream.info.period_frames.max(1)) / f64::from(stream.format.sample_rate),
    );
    let mut last = Instant::now();
    let mut wakes = 0u64;
    loop {
        match wait(&handles, 2_000) {
            Some(0) => return Exit::Stop,
            Some(1) => {
                let woke = Instant::now();
                match fill(renderer) {
                    Ok(padding) => {
                        wakes += 1;
                        // Skip the first wakes while the stream settles.
                        if wakes > 4 {
                            if padding == 0 {
                                stats.empty_wakes.fetch_add(1, Ordering::Relaxed);
                            }
                            if woke - last > period * 3 / 2 {
                                stats.late_wakes.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        stats.callbacks.fetch_add(1, Ordering::Relaxed);
                        stats
                            .padding_frames
                            .fetch_add(u64::from(padding), Ordering::Relaxed);
                        stats.interval.record(woke - last);
                        stats.render.record(woke.elapsed());
                        last = woke;
                    }
                    Err(AUDCLNT_E_DEVICE_INVALIDATED) => return Exit::Reopen,
                    Err(_) => {}
                }
            }
            Some(2) => {
                // Something changed: reopen only if the default device is another one.
                let current = enumerator
                    .and_then(|e| default_device(e).ok())
                    .map(|(_, id)| id);
                if current.as_deref() != Some(stream.device_id.as_str()) {
                    return Exit::Reopen;
                }
            }
            _ => {
                // No event for 2 s: the device stalled or disappeared.
                let mut padding = 0;
                // SAFETY: valid client and out pointer.
                if check(unsafe { (c.GetCurrentPadding)(stream.client.raw(), &mut padding) })
                    .is_err()
                {
                    return Exit::Reopen;
                }
            }
        }
    }
}

/// No device: render at the nominal rate on a timer and discard, retrying every second.
fn silent_loop(
    renderer: &mut Renderer,
    control: &Control,
    changed: Option<HANDLE>,
    scratch: &mut Vec<f32>,
) -> Exit {
    const CHUNK: usize = 480;
    if scratch.len() < CHUNK * 2 {
        scratch.resize(CHUNK * 2, 0.0);
    }
    let rate = f64::from(renderer.sample_rate());
    let start = Instant::now();
    let mut rendered = 0u64;
    let mut handles = vec![control.stop.0];
    if let Some(h) = changed {
        handles.push(h);
    }
    loop {
        match wait(&handles, 10) {
            Some(0) => return Exit::Stop,
            Some(_) => return Exit::Retry,
            None => {}
        }
        let due = (start.elapsed().as_secs_f64() * rate) as u64;
        while rendered < due {
            let n = ((due - rendered) as usize).min(CHUNK);
            renderer.render(&mut scratch[..n * 2], 2);
            rendered += n as u64;
        }
        if start.elapsed() >= Duration::from_secs(1) {
            return Exit::Retry;
        }
    }
}
