//! The few COM interfaces WASAPI needs, declared by hand over `windows-sys` types.
//!
//! Vtable layouts follow the Windows SDK headers (`mmdeviceapi.h`, `audioclient.h`,
//! `propsys.h`); slots we never call are `usize` placeholders of the same size. Each
//! interface pointer is wrapped in [`Com`], which releases it on drop.

#![allow(non_snake_case)]

use core::ffi::c_void;
use core::ptr::NonNull;
use windows_sys::Win32::Foundation::{HANDLE, PROPERTYKEY};
use windows_sys::core::{BOOL, GUID, HRESULT};

pub(crate) const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
pub(crate) const IID_IMM_DEVICE_ENUMERATOR: GUID =
    GUID::from_u128(0xa95664d2_9614_4f35_a746_de8db63617e6);
pub(crate) const IID_IMM_NOTIFICATION_CLIENT: GUID =
    GUID::from_u128(0x7991eec9_7e89_4d85_8390_6c703cec60c0);
pub(crate) const IID_IAUDIO_CLIENT: GUID = GUID::from_u128(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);
pub(crate) const IID_IAUDIO_CLIENT3: GUID = GUID::from_u128(0x7ed4ee07_8e67_4cd4_8c1a_2b7a5987ad42);
pub(crate) const IID_IAUDIO_RENDER_CLIENT: GUID =
    GUID::from_u128(0xf294acfc_3146_4483_a7bf_addca7c260e2);
pub(crate) const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

pub(crate) type This = *mut c_void;

#[repr(C)]
pub(crate) struct IUnknownVtbl {
    pub QueryInterface: unsafe extern "system" fn(This, *const GUID, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "system" fn(This) -> u32,
    pub Release: unsafe extern "system" fn(This) -> u32,
}

#[repr(C)]
pub(crate) struct IMMDeviceEnumeratorVtbl {
    pub base: IUnknownVtbl,
    pub EnumAudioEndpoints: usize,
    pub GetDefaultAudioEndpoint:
        unsafe extern "system" fn(This, i32, i32, *mut *mut c_void) -> HRESULT,
    pub GetDevice: usize,
    pub RegisterEndpointNotificationCallback: unsafe extern "system" fn(This, This) -> HRESULT,
    pub UnregisterEndpointNotificationCallback: unsafe extern "system" fn(This, This) -> HRESULT,
}

#[repr(C)]
pub(crate) struct IMMDeviceVtbl {
    pub base: IUnknownVtbl,
    pub Activate: unsafe extern "system" fn(
        This,
        *const GUID,
        u32,
        *const c_void,
        *mut *mut c_void,
    ) -> HRESULT,
    pub OpenPropertyStore: unsafe extern "system" fn(This, u32, *mut *mut c_void) -> HRESULT,
    pub GetId: unsafe extern "system" fn(This, *mut *mut u16) -> HRESULT,
    pub GetState: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
}

/// `PROPVARIANT` as far as we read it: the type tag and the first pointer of the union.
#[repr(C)]
pub(crate) struct PropVariant {
    pub vt: u16,
    pub reserved: [u16; 3],
    pub data: [usize; 2],
}

pub(crate) const VT_LPWSTR: u16 = 31;

#[repr(C)]
pub(crate) struct IPropertyStoreVtbl {
    pub base: IUnknownVtbl,
    pub GetCount: usize,
    pub GetAt: usize,
    pub GetValue: unsafe extern "system" fn(This, *const PROPERTYKEY, *mut PropVariant) -> HRESULT,
    pub SetValue: usize,
    pub Commit: usize,
}

#[repr(C)]
pub(crate) struct IAudioClientVtbl {
    pub base: IUnknownVtbl,
    pub Initialize:
        unsafe extern "system" fn(This, i32, u32, i64, i64, *const c_void, *const GUID) -> HRESULT,
    pub GetBufferSize: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
    pub GetStreamLatency: unsafe extern "system" fn(This, *mut i64) -> HRESULT,
    pub GetCurrentPadding: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
    pub IsFormatSupported: usize,
    pub GetMixFormat: unsafe extern "system" fn(This, *mut *mut c_void) -> HRESULT,
    pub GetDevicePeriod: unsafe extern "system" fn(This, *mut i64, *mut i64) -> HRESULT,
    pub Start: unsafe extern "system" fn(This) -> HRESULT,
    pub Stop: unsafe extern "system" fn(This) -> HRESULT,
    pub Reset: unsafe extern "system" fn(This) -> HRESULT,
    pub SetEventHandle: unsafe extern "system" fn(This, HANDLE) -> HRESULT,
    pub GetService: unsafe extern "system" fn(This, *const GUID, *mut *mut c_void) -> HRESULT,
}

/// `AudioClientProperties` (`audioclient.h`).
#[repr(C)]
pub(crate) struct AudioClientProperties {
    pub cbSize: u32,
    pub bIsOffload: BOOL,
    pub eCategory: i32,
    pub Options: u32,
}

#[repr(C)]
pub(crate) struct IAudioClient3Vtbl {
    pub base: IAudioClientVtbl,
    // IAudioClient2
    pub IsOffloadCapable: usize,
    pub SetClientProperties:
        unsafe extern "system" fn(This, *const AudioClientProperties) -> HRESULT,
    pub GetBufferSizeLimits: usize,
    // IAudioClient3
    pub GetSharedModeEnginePeriod: unsafe extern "system" fn(
        This,
        *const c_void,
        *mut u32,
        *mut u32,
        *mut u32,
        *mut u32,
    ) -> HRESULT,
    pub GetCurrentSharedModeEnginePeriod:
        unsafe extern "system" fn(This, *mut *mut c_void, *mut u32) -> HRESULT,
    pub InitializeSharedAudioStream:
        unsafe extern "system" fn(This, u32, u32, *const c_void, *const GUID) -> HRESULT,
}

#[repr(C)]
pub(crate) struct IAudioRenderClientVtbl {
    pub base: IUnknownVtbl,
    pub GetBuffer: unsafe extern "system" fn(This, u32, *mut *mut u8) -> HRESULT,
    pub ReleaseBuffer: unsafe extern "system" fn(This, u32, u32) -> HRESULT,
}

#[repr(C)]
pub(crate) struct IMMNotificationClientVtbl {
    pub base: IUnknownVtbl,
    pub OnDeviceStateChanged: unsafe extern "system" fn(This, *const u16, u32) -> HRESULT,
    pub OnDeviceAdded: unsafe extern "system" fn(This, *const u16) -> HRESULT,
    pub OnDeviceRemoved: unsafe extern "system" fn(This, *const u16) -> HRESULT,
    pub OnDefaultDeviceChanged: unsafe extern "system" fn(This, i32, i32, *const u16) -> HRESULT,
    pub OnPropertyValueChanged: unsafe extern "system" fn(This, *const u16, PROPERTYKEY) -> HRESULT,
}

/// An owned COM interface pointer whose vtable is `V` (a vtable that starts with
/// [`IUnknownVtbl`]). Released on drop.
pub(crate) struct Com<V> {
    ptr: NonNull<*const V>,
}

impl<V> Com<V> {
    /// Takes ownership of one reference.
    ///
    /// # Safety
    /// `raw` is null or a COM interface pointer whose vtable has layout `V`.
    pub(crate) unsafe fn from_raw(raw: *mut c_void) -> Option<Self> {
        NonNull::new(raw.cast::<*const V>()).map(|ptr| Self { ptr })
    }

    pub(crate) fn raw(&self) -> This {
        self.ptr.as_ptr().cast()
    }

    pub(crate) fn vtbl(&self) -> &V {
        // SAFETY: a live COM object starts with its vtable pointer, and `from_raw`'s
        // contract says that vtable has layout `V`. The vtable is static for the object's
        // life, which `self` keeps alive.
        unsafe { &**self.ptr.as_ptr() }
    }

    /// Reinterprets as another vtable layout of the same object.
    ///
    /// # Safety
    /// The object's vtable must have layout `W` (for example, it was obtained for a derived
    /// interface's IID).
    pub(crate) unsafe fn cast<W>(&self) -> &Com<W> {
        // SAFETY: `Com<V>` and `Com<W>` have the same layout (one pointer); the caller
        // guarantees the vtable layout.
        unsafe { &*(self as *const Self).cast::<Com<W>>() }
    }
}

impl<V> Drop for Com<V> {
    fn drop(&mut self) {
        // SAFETY: every vtable used with `Com` starts with `IUnknownVtbl`; we own one
        // reference and give it back once.
        unsafe {
            let unknown = &**self.ptr.as_ptr().cast::<*const IUnknownVtbl>();
            (unknown.Release)(self.raw());
        }
    }
}

/// GUID equality (`windows-sys` GUIDs do not implement `PartialEq`).
pub(crate) fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// `Err(hr)` for failing `HRESULT`s.
pub(crate) fn check(hr: HRESULT) -> Result<(), HRESULT> {
    if hr < 0 { Err(hr) } else { Ok(()) }
}

/// Converts a NUL-terminated UTF-16 string from the system to a `String`.
///
/// # Safety
/// `p` is null or points to a NUL-terminated UTF-16 string.
pub(crate) unsafe fn wide_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    // SAFETY: the string is NUL-terminated, so every index up to the NUL is readable.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` elements are readable (just scanned).
    String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(p, len) })
}
