//! What the monitor showing a window can display (ADR 0038): whether Windows runs it in
//! HDR, the luminance it gives SDR white, the display's luminance range and its refresh
//! rate.
//!
//! Each source answers what only it knows:
//!
//! - `QueryDisplayConfig` and `DisplayConfigGetDeviceInfo`: whether the display supports
//!   HDR and runs in it, and the SDR white level (the "SDR content brightness" slider);
//! - DXGI, `IDXGIOutput6::GetDesc1`: the luminance range, as the display's EDID states it
//!   or as the Windows HDR Calibration app measured it;
//! - `EnumDisplaySettingsW`: the refresh rate.
//!
//! Windows does not announce every change: turning HDR on or off sends `WM_DISPLAYCHANGE`,
//! moving the SDR brightness slider sends nothing. The window thread asks again on display
//! changes, when a window moves to another monitor and once a second, and reports what
//! changed as [`WindowEventKind::Display`](crate::window::WindowEventKind::Display).
//!
//! `windows-sys` has no DXGI bindings; the five calls needed go through the COM vtables.

use core::ffi::c_void;
use core::ptr::{NonNull, null_mut};
use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_DEVICE_INFO_TYPE, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HWND, LUID, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows_sys::core::GUID;

/// Luminance of SDR white when Windows does not say otherwise: sRGB's reference white,
/// and 1.0 in scRGB.
pub const SDR_WHITE_NITS: f32 = 80.0;

/// What a display can show, and how Windows drives it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayInfo {
    /// The monitor (an `HMONITOR`), to tell displays apart.
    pub monitor: isize,
    /// Refresh rate in whole hertz as Windows reports the current mode (239 for a
    /// 239.76 Hz panel); `None` when the monitor reports its hardware default.
    pub refresh_hz: Option<u32>,
    /// The display can run in HDR.
    pub hdr_supported: bool,
    /// Windows runs the display in HDR ("Use HDR" is on): output may be brighter than SDR
    /// white, up to [`max_nits`](Self::max_nits).
    pub hdr_enabled: bool,
    /// Luminance Windows gives SDR white (sRGB 1.0) in nits: the "SDR content brightness"
    /// slider while in HDR; [`SDR_WHITE_NITS`] otherwise.
    pub sdr_white_nits: f32,
    /// Peak luminance of a small area in nits; 0 when unknown.
    pub max_nits: f32,
    /// Luminance the display sustains over the whole screen in nits; 0 when unknown.
    pub max_full_frame_nits: f32,
    /// Black level in nits.
    pub min_nits: f32,
}

/// Asks Windows about displays. Keeps its DXGI factory and buffers between queries: the
/// factory is replaced only when the display configuration changed.
pub struct DisplayQuery {
    factory: Option<Com>,
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

// SAFETY: DXGI objects are free-threaded; the query is used by one thread at a time
// (`&mut self`).
unsafe impl Send for DisplayQuery {}

impl core::fmt::Debug for DisplayQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DisplayQuery")
            .field("factory", &self.factory.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for DisplayQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl DisplayQuery {
    /// A query; the DXGI factory is created on first use.
    pub fn new() -> Self {
        Self {
            factory: None,
            paths: Vec::new(),
            modes: Vec::new(),
        }
    }

    /// The monitor showing most of window `hwnd` (a
    /// [`WindowHandle::hwnd`](crate::window::WindowHandle::hwnd)); `None` when Windows
    /// cannot say (for example in a session without a desktop).
    pub fn window(&mut self, hwnd: isize) -> Option<DisplayInfo> {
        self.monitor(monitor_of(hwnd))
    }

    /// The monitor `monitor` (an `HMONITOR`, as in [`DisplayInfo::monitor`]).
    pub fn monitor(&mut self, monitor: isize) -> Option<DisplayInfo> {
        let device = device_name(monitor)?;
        let color = self.advanced_color(&device);
        let range = self.luminance(monitor);
        Some(DisplayInfo {
            monitor,
            refresh_hz: device_refresh_hz(&device),
            // Without DisplayConfig (it should not fail), DXGI's color space still tells
            // whether the output runs in HDR.
            hdr_supported: color.map_or(range.is_some_and(|r| r.hdr), |c| c.supported),
            hdr_enabled: color.map_or(range.is_some_and(|r| r.hdr), |c| c.enabled),
            sdr_white_nits: color.map_or(SDR_WHITE_NITS, |c| c.sdr_white_nits),
            max_nits: range.map_or(0.0, |r| r.max),
            max_full_frame_nits: range.map_or(0.0, |r| r.full_frame),
            min_nits: range.map_or(0.0, |r| r.min),
        })
    }

    /// HDR state and SDR white level of the display that shows GDI device `device`.
    fn advanced_color(&mut self, device: &[u16; 32]) -> Option<ColorState> {
        let (adapter, id) = self.target_of(device)?;
        let mut info2 = AdvancedColorInfo2 {
            header: header::<AdvancedColorInfo2>(GET_ADVANCED_COLOR_INFO_2, adapter, id),
            flags: 0,
            color_encoding: 0,
            bits_per_color_channel: 0,
            active_color_mode: 0,
        };
        // SAFETY: the header states the structure's type and size.
        let (supported, enabled) = if unsafe { DisplayConfigGetDeviceInfo(&mut info2.header) } == 0
        {
            (
                info2.flags & HDR_SUPPORTED != 0,
                info2.active_color_mode == ADVANCED_COLOR_MODE_HDR,
            )
        } else {
            // Before Windows 11 24H2: "advanced color" is HDR unless it is wide color
            // forced onto an SDR display.
            let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
                header: header::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>(
                    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                    adapter,
                    id,
                ),
                ..Default::default()
            };
            // SAFETY: as above.
            if unsafe { DisplayConfigGetDeviceInfo(&mut info.header) } != 0 {
                return None;
            }
            // SAFETY: every bit pattern is a valid `u32`.
            let bits = unsafe { info.Anonymous.value };
            (bits & 1 != 0, bits & 0b10 != 0 && bits & 0b100 == 0)
        };
        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: header::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(
                DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                adapter,
                id,
            ),
            SDRWhiteLevel: 0,
        };
        // SAFETY: as above.
        let white_ok = unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } == 0;
        // The level is in thousandths of 80 nits.
        let sdr_white_nits = if white_ok && white.SDRWhiteLevel > 0 {
            white.SDRWhiteLevel as f32 * SDR_WHITE_NITS / 1000.0
        } else {
            SDR_WHITE_NITS
        };
        Some(ColorState {
            supported,
            enabled,
            sdr_white_nits,
        })
    }

    /// The display target (adapter, id) behind GDI device `device`.
    fn target_of(&mut self, device: &[u16; 32]) -> Option<(LUID, u32)> {
        // The configuration can change between sizing the buffers and filling them.
        for _ in 0..4 {
            let (mut path_count, mut mode_count) = (0u32, 0u32);
            // SAFETY: out pointers to locals.
            let sized = unsafe {
                GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
            };
            if sized != ERROR_SUCCESS {
                return None;
            }
            self.paths
                .resize(path_count as usize, DISPLAYCONFIG_PATH_INFO::default());
            self.modes
                .resize(mode_count as usize, DISPLAYCONFIG_MODE_INFO::default());
            // SAFETY: the buffers hold the element counts passed; the topology id is only
            // for `QDC_DATABASE_CURRENT` and must be null otherwise.
            let result = unsafe {
                QueryDisplayConfig(
                    QDC_ONLY_ACTIVE_PATHS,
                    &mut path_count,
                    self.paths.as_mut_ptr(),
                    &mut mode_count,
                    self.modes.as_mut_ptr(),
                    null_mut(),
                )
            };
            if result == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if result != ERROR_SUCCESS {
                return None;
            }
            self.paths.truncate(path_count as usize);
            return self.paths.iter().find_map(|path| {
                let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: header::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(
                        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        path.sourceInfo.adapterId,
                        path.sourceInfo.id,
                    ),
                    viewGdiDeviceName: [0; 32],
                };
                // SAFETY: the header states the structure's type and size.
                let named = unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } == 0;
                (named && same_name(&source.viewGdiDeviceName, device))
                    .then_some((path.targetInfo.adapterId, path.targetInfo.id))
            });
        }
        None
    }

    /// Turns HDR on or off for `monitor` (an `HMONITOR`), like the "Use HDR" switch in
    /// Settings and Win+Alt+B: a user setting, changed for every application. For
    /// settings menus and tests. The display switches modes and goes dark for a moment;
    /// windows get `WM_DISPLAYCHANGE`. Returns whether Windows accepted the request.
    pub fn set_hdr(&mut self, monitor: isize, enabled: bool) -> bool {
        let Some(device) = device_name(monitor) else {
            return false;
        };
        let Some((adapter, id)) = self.target_of(&device) else {
            return false;
        };
        let mut state = SetState {
            header: header::<SetState>(SET_HDR_STATE, adapter, id),
            value: u32::from(enabled),
        };
        // SAFETY: the header states the structure's type and size.
        if unsafe { DisplayConfigSetDeviceInfo(&state.header) } == 0 {
            return true;
        }
        // Before Windows 11 24H2, HDR is "advanced color" on an HDR display.
        state.header.r#type = DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE;
        // SAFETY: as above; both requests share the layout.
        unsafe { DisplayConfigSetDeviceInfo(&state.header) == 0 }
    }

    /// Luminance range of `monitor` from its DXGI output.
    fn luminance(&mut self, monitor: isize) -> Option<Range> {
        let factory = self.factory()?;
        for a in 0.. {
            let adapter = factory.child(FACTORY1_ENUM_ADAPTERS1, a)?;
            for o in 0.. {
                let Some(output) = adapter.child(ADAPTER_ENUM_OUTPUTS, o) else {
                    break;
                };
                let Some(output6) = output.query(&IID_DXGI_OUTPUT6) else {
                    continue;
                };
                let Some(desc) = output6.desc1() else {
                    continue;
                };
                if desc.monitor as isize == monitor {
                    return Some(Range {
                        hdr: desc.color_space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                        min: desc.min_luminance,
                        max: desc.max_luminance,
                        full_frame: desc.max_full_frame_luminance,
                    });
                }
            }
        }
        None
    }

    /// The DXGI factory, replaced when it no longer describes the adapters and outputs.
    fn factory(&mut self) -> Option<&Com> {
        if let Some(factory) = &self.factory
            && !factory.is_current()
        {
            self.factory = None;
        }
        if self.factory.is_none() {
            let mut raw = null_mut();
            // SAFETY: a documented entry point with an out pointer to a local.
            let hr = unsafe { CreateDXGIFactory1(&IID_DXGI_FACTORY1, &mut raw) };
            if hr < 0 {
                return None;
            }
            // SAFETY: an owned interface pointer from a successful call.
            self.factory = unsafe { Com::from_raw(raw) };
        }
        self.factory.as_ref()
    }
}

/// The monitor showing most of window `hwnd`, as an integer (0 if none).
pub(crate) fn monitor_of(hwnd: isize) -> isize {
    // SAFETY: a plain query; any window handle is accepted.
    unsafe { MonitorFromWindow(hwnd as HWND, MONITOR_DEFAULTTONEAREST) as isize }
}

/// GDI device name of `monitor` (`\\.\DISPLAY1`), NUL-terminated.
pub(crate) fn device_name(monitor: isize) -> Option<[u16; 32]> {
    if monitor == 0 {
        return None;
    }
    // SAFETY: plain query; the structure carries its size, as the API requires.
    unsafe {
        let mut info: MONITORINFOEXW = core::mem::zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor as HMONITOR, (&raw mut info).cast::<MONITORINFO>()) == 0 {
            return None;
        }
        Some(info.szDevice)
    }
}

/// Refresh rate of the current mode of GDI device `device`.
pub(crate) fn device_refresh_hz(device: &[u16; 32]) -> Option<u32> {
    // SAFETY: plain query; the structure carries its size; `device` is NUL-terminated.
    unsafe {
        let mut mode: DEVMODEW = core::mem::zeroed();
        mode.dmSize = size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(device.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) == 0 {
            return None;
        }
        // 0 and 1 stand for "the hardware's default rate".
        (mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
    }
}

/// Whether two NUL-terminated device names are equal.
fn same_name(a: &[u16; 32], b: &[u16; 32]) -> bool {
    let len = |s: &[u16; 32]| s.iter().position(|&c| c == 0).unwrap_or(s.len());
    a[..len(a)] == b[..len(b)]
}

fn header<T>(
    kind: DISPLAYCONFIG_DEVICE_INFO_TYPE,
    adapter: LUID,
    id: u32,
) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size_of::<T>() as u32,
        adapterId: adapter,
        id,
    }
}

#[derive(Clone, Copy)]
struct ColorState {
    supported: bool,
    enabled: bool,
    sdr_white_nits: f32,
}

#[derive(Clone, Copy)]
struct Range {
    hdr: bool,
    min: f32,
    max: f32,
    full_frame: f32,
}

// ---------------------------------------------------------------------------------------
// DisplayConfig beyond the bindings
// ---------------------------------------------------------------------------------------

/// `DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO_2` (Windows 11 24H2).
const GET_ADVANCED_COLOR_INFO_2: DISPLAYCONFIG_DEVICE_INFO_TYPE = 15;
/// `highDynamicRangeSupported` in `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO_2`.
const HDR_SUPPORTED: u32 = 1 << 4;
/// `DISPLAYCONFIG_ADVANCED_COLOR_MODE_HDR`.
const ADVANCED_COLOR_MODE_HDR: i32 = 2;
/// `DISPLAYCONFIG_DEVICE_INFO_SET_HDR_STATE` (Windows 11 24H2).
const SET_HDR_STATE: DISPLAYCONFIG_DEVICE_INFO_TYPE = 16;

/// `DISPLAYCONFIG_SET_HDR_STATE`, and `DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE` before it:
/// one flag in bit 0.
#[repr(C)]
struct SetState {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    value: u32,
}

/// `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO_2`: unlike the older structure, it tells HDR
/// apart from wide color on an SDR display (automatic color management).
#[repr(C)]
struct AdvancedColorInfo2 {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
    color_encoding: i32,
    bits_per_color_channel: u32,
    /// `DISPLAYCONFIG_ADVANCED_COLOR_MODE`: SDR, WCG or HDR.
    active_color_mode: i32,
}

// ---------------------------------------------------------------------------------------
// DXGI through COM vtables
// ---------------------------------------------------------------------------------------

#[link(name = "dxgi.dll", kind = "raw-dylib", modifiers = "+verbatim")]
unsafe extern "system" {
    fn CreateDXGIFactory1(riid: *const GUID, factory: *mut *mut c_void) -> i32;
}

const IID_DXGI_FACTORY1: GUID = GUID::from_u128(0x770a_ae78_f26f_4dba_a829_253c_83d1_b387);
const IID_DXGI_OUTPUT6: GUID = GUID::from_u128(0x0683_46e8_aaec_4b84_add7_137f_513f_77a1);
/// `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`: the output runs in HDR10.
const DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020: i32 = 12;

// Vtable slots: IUnknown (0-2), IDXGIObject (3-6), then each interface in order.
const QUERY_INTERFACE: usize = 0;
const RELEASE: usize = 2;
/// `IDXGIFactory1::EnumAdapters1` (after IDXGIFactory's 7-11).
const FACTORY1_ENUM_ADAPTERS1: usize = 12;
/// `IDXGIFactory1::IsCurrent`.
const FACTORY1_IS_CURRENT: usize = 13;
/// `IDXGIAdapter::EnumOutputs`.
const ADAPTER_ENUM_OUTPUTS: usize = 7;
/// `IDXGIOutput6::GetDesc1` (after IDXGIOutput's 7-18 and IDXGIOutput1-5's 19-26).
const OUTPUT6_GET_DESC1: usize = 27;

type QueryInterfaceFn =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type EnumFn = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32;
type IsCurrentFn = unsafe extern "system" fn(*mut c_void) -> i32;
type GetDesc1Fn = unsafe extern "system" fn(*mut c_void, *mut OutputDesc1) -> i32;

/// `DXGI_OUTPUT_DESC1`.
#[repr(C)]
struct OutputDesc1 {
    device_name: [u16; 32],
    desktop_coordinates: RECT,
    attached_to_desktop: i32,
    rotation: i32,
    monitor: HMONITOR,
    bits_per_color: u32,
    color_space: i32,
    red_primary: [f32; 2],
    green_primary: [f32; 2],
    blue_primary: [f32; 2],
    white_point: [f32; 2],
    min_luminance: f32,
    max_luminance: f32,
    max_full_frame_luminance: f32,
}

const _: () = assert!(size_of::<OutputDesc1>() == 152);

/// An owned COM interface pointer, released on drop.
struct Com(NonNull<c_void>);

impl Com {
    /// # Safety
    ///
    /// `raw` is null or an interface pointer whose reference the caller owns.
    unsafe fn from_raw(raw: *mut c_void) -> Option<Self> {
        NonNull::new(raw).map(Self)
    }

    /// Entry `slot` of the object's vtable.
    ///
    /// # Safety
    ///
    /// The interface the pointer has must have at least `slot + 1` methods.
    unsafe fn slot(&self, slot: usize) -> *const c_void {
        // SAFETY: a COM object starts with a pointer to its vtable, an array of function
        // pointers; the caller guarantees the index.
        unsafe { *(*self.0.as_ptr().cast::<*const *const c_void>()).add(slot) }
    }

    /// `EnumAdapters1` or `EnumOutputs` (`slot`): child `index`, `None` past the last.
    fn child(&self, slot: usize, index: u32) -> Option<Com> {
        let mut raw = null_mut();
        // SAFETY: the callers pass the slot of an enumeration method of this interface,
        // which has this signature.
        let hr = unsafe {
            let method: EnumFn = core::mem::transmute(self.slot(slot));
            method(self.0.as_ptr(), index, &mut raw)
        };
        if hr < 0 {
            return None;
        }
        // SAFETY: an owned interface pointer from a successful call.
        unsafe { Com::from_raw(raw) }
    }

    /// `QueryInterface`.
    fn query(&self, iid: &GUID) -> Option<Com> {
        let mut raw = null_mut();
        // SAFETY: slot 0 of every COM interface is `QueryInterface`.
        let hr = unsafe {
            let method: QueryInterfaceFn = core::mem::transmute(self.slot(QUERY_INTERFACE));
            method(self.0.as_ptr(), iid, &mut raw)
        };
        if hr < 0 {
            return None;
        }
        // SAFETY: an owned interface pointer from a successful call.
        unsafe { Com::from_raw(raw) }
    }

    /// `IDXGIFactory1::IsCurrent`: whether the adapters and outputs it enumerates are
    /// still the system's (for a factory).
    fn is_current(&self) -> bool {
        // SAFETY: only called on the factory, an `IDXGIFactory1`.
        unsafe {
            let method: IsCurrentFn = core::mem::transmute(self.slot(FACTORY1_IS_CURRENT));
            method(self.0.as_ptr()) != 0
        }
    }

    /// `IDXGIOutput6::GetDesc1` (for an output queried as `IDXGIOutput6`).
    fn desc1(&self) -> Option<OutputDesc1> {
        let mut desc = core::mem::MaybeUninit::<OutputDesc1>::zeroed();
        // SAFETY: only called on `IDXGIOutput6` pointers; `desc` is a valid out pointer.
        let hr = unsafe {
            let method: GetDesc1Fn = core::mem::transmute(self.slot(OUTPUT6_GET_DESC1));
            method(self.0.as_ptr(), desc.as_mut_ptr())
        };
        // SAFETY: all-zero bytes are a valid `OutputDesc1`, and a successful call filled it.
        (hr >= 0).then(|| unsafe { desc.assume_init() })
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: slot 2 of every COM interface is `Release`; the reference is owned.
        unsafe {
            let release: ReleaseFn = core::mem::transmute(self.slot(RELEASE));
            release(self.0.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    #[test]
    fn describes_the_desktop_monitor() {
        // SAFETY: a plain query.
        let desktop = unsafe { GetDesktopWindow() } as isize;
        let mut query = DisplayQuery::new();
        let start = Instant::now();
        let Some(info) = query.window(desktop) else {
            println!("no display to describe (a session without a desktop?): skipped");
            return;
        };
        let first = start.elapsed();
        let start = Instant::now();
        for _ in 0..50 {
            assert_eq!(query.window(desktop), Some(info));
        }
        let again = start.elapsed() / 50;
        println!("{info:?}");
        println!("first query {first:?} (creates the DXGI factory), then {again:?} each");
        assert!(info.hdr_supported || !info.hdr_enabled);
        assert!(info.sdr_white_nits >= 40.0 && info.sdr_white_nits <= 1000.0);
        assert!(info.max_nits == 0.0 || info.min_nits <= info.max_nits);
    }

    #[test]
    fn device_names_compare_up_to_the_terminator() {
        let name = |s: &str| {
            let mut out = [0u16; 32];
            for (o, c) in out.iter_mut().zip(s.encode_utf16()) {
                *o = c;
            }
            out
        };
        let mut junk = name(r"\\.\DISPLAY1");
        junk[20] = 7;
        assert!(same_name(&name(r"\\.\DISPLAY1"), &junk));
        assert!(!same_name(&name(r"\\.\DISPLAY1"), &name(r"\\.\DISPLAY12")));
    }
}
