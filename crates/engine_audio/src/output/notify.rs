//! An `IMMNotificationClient` that signals an event on endpoint changes (default device
//! switched, device added, removed or disabled). The output thread wakes on it and decides
//! whether to reopen.

use super::com::{
    Com, IID_IMM_NOTIFICATION_CLIENT, IID_IUNKNOWN, IMMNotificationClientVtbl, IUnknownVtbl, This,
    same_guid,
};
use core::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::Foundation::{CloseHandle, E_NOINTERFACE, HANDLE, PROPERTYKEY, S_OK};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};
use windows_sys::core::{GUID, HRESULT};

#[repr(C)]
struct Notifier {
    vtbl: *const IMMNotificationClientVtbl,
    refs: AtomicU32,
    /// Auto-reset event, owned by the notifier: it lives as long as any reference, so the
    /// system can never signal a closed handle.
    event: HANDLE,
}

static VTBL: IMMNotificationClientVtbl = IMMNotificationClientVtbl {
    base: IUnknownVtbl {
        QueryInterface: query_interface,
        AddRef: add_ref,
        Release: release,
    },
    OnDeviceStateChanged: on_state_changed,
    OnDeviceAdded: on_device,
    OnDeviceRemoved: on_device,
    OnDefaultDeviceChanged: on_default_changed,
    OnPropertyValueChanged: on_property_changed,
};

unsafe extern "system" fn query_interface(
    this: This,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: COM passes a valid IID and out pointer.
    let (iid, out) = unsafe { (&*iid, &mut *out) };
    if same_guid(iid, &IID_IUNKNOWN) || same_guid(iid, &IID_IMM_NOTIFICATION_CLIENT) {
        *out = this;
        // SAFETY: `this` is a live notifier.
        unsafe { add_ref(this) };
        S_OK
    } else {
        *out = core::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn add_ref(this: This) -> u32 {
    // SAFETY: `this` points to a live `Notifier` (COM holds a reference to call us).
    unsafe { &*this.cast::<Notifier>() }
        .refs
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe extern "system" fn release(this: This) -> u32 {
    // SAFETY: as in `add_ref`.
    let left = unsafe { &*this.cast::<Notifier>() }
        .refs
        .fetch_sub(1, Ordering::AcqRel)
        - 1;
    if left == 0 {
        // SAFETY: the last reference is gone; the notifier was created by `Box::into_raw`
        // in `create` and is freed once, together with its event.
        unsafe {
            let notifier = Box::from_raw(this.cast::<Notifier>());
            CloseHandle(notifier.event);
        }
    }
    left
}

fn signal(this: This) -> HRESULT {
    // SAFETY: COM calls us with a live notifier; its event is open while it lives.
    unsafe { SetEvent((*this.cast::<Notifier>()).event) };
    S_OK
}

unsafe extern "system" fn on_state_changed(this: This, _: *const u16, _: u32) -> HRESULT {
    signal(this)
}

unsafe extern "system" fn on_device(this: This, _: *const u16) -> HRESULT {
    signal(this)
}

unsafe extern "system" fn on_default_changed(
    this: This,
    flow: i32,
    role: i32,
    _: *const u16,
) -> HRESULT {
    use windows_sys::Win32::Media::Audio::{eConsole, eRender};
    if flow == eRender && role == eConsole {
        signal(this)
    } else {
        S_OK
    }
}

unsafe extern "system" fn on_property_changed(_: This, _: *const u16, _: PROPERTYKEY) -> HRESULT {
    S_OK
}

/// Creates a notifier with one reference.
pub(crate) fn create() -> io::Result<Com<IMMNotificationClientVtbl>> {
    // SAFETY: an unnamed auto-reset event with default security.
    let event = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let raw = Box::into_raw(Box::new(Notifier {
        vtbl: &VTBL,
        refs: AtomicU32::new(1),
        event,
    }));
    // SAFETY: a non-null object whose vtable is `IMMNotificationClientVtbl`; we hand over
    // its only reference.
    unsafe { Com::from_raw(raw.cast()) }.ok_or_else(|| io::Error::other("null notifier"))
}

/// The event the notifier signals.
pub(crate) fn event(notifier: &Com<IMMNotificationClientVtbl>) -> HANDLE {
    // SAFETY: `create` made this object a `Notifier`; `notifier` keeps it alive.
    unsafe { (*notifier.raw().cast::<Notifier>()).event }
}
