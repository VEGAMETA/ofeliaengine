//! The Windows clipboard's text (`CF_UNICODETEXT`): what text fields copy, cut and paste.
//!
//! ```no_run
//! if engine_platform::clipboard::set_text("Door") {
//!     assert_eq!(engine_platform::clipboard::text().as_deref(), Some("Door"));
//! }
//! ```

/// The clipboard's text; `None` when it holds none or another program has it open.
pub fn text() -> Option<String> {
    imp::text()
}

/// Puts `text` on the clipboard; false when another program has it open.
pub fn set_text(text: &str) -> bool {
    imp::set_text(text)
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };

    /// `CF_UNICODETEXT`: UTF-16, ending in a zero.
    const CF_UNICODETEXT: u32 = 13;

    /// The clipboard opened, closed again when dropped.
    struct Open;

    impl Open {
        fn new() -> Option<Self> {
            // SAFETY: no owner window; the clipboard is closed again by `Drop`.
            (unsafe { OpenClipboard(core::ptr::null_mut()) } != 0).then_some(Self)
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: opened by `new`.
            unsafe { CloseClipboard() };
        }
    }

    pub(super) fn text() -> Option<String> {
        // SAFETY: a query of the format list.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
            return None;
        }
        let _open = Open::new()?;
        // SAFETY: the clipboard is open; the handle stays valid until it closes, and is
        // read inside its lock, within its size.
        unsafe {
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                return None;
            }
            let units = GlobalSize(handle) / 2;
            let data = GlobalLock(handle).cast::<u16>();
            if data.is_null() {
                return None;
            }
            let all = core::slice::from_raw_parts(data, units);
            let len = all.iter().position(|&u| u == 0).unwrap_or(units);
            let text = String::from_utf16_lossy(&all[..len]);
            GlobalUnlock(handle);
            Some(text)
        }
    }

    pub(super) fn set_text(text: &str) -> bool {
        let units: Vec<u16> = text.encode_utf16().chain(core::iter::once(0)).collect();
        let Some(_open) = Open::new() else {
            return false;
        };
        // SAFETY: the clipboard is open; the memory is written inside its lock and, once
        // given to the clipboard, owned by it (freed here only when it was refused).
        unsafe {
            if EmptyClipboard() == 0 {
                return false;
            }
            let memory = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
            if memory.is_null() {
                return false;
            }
            let data = GlobalLock(memory).cast::<u16>();
            if data.is_null() {
                GlobalFree(memory);
                return false;
            }
            core::ptr::copy_nonoverlapping(units.as_ptr(), data, units.len());
            GlobalUnlock(memory);
            if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
                GlobalFree(memory);
                return false;
            }
        }
        true
    }
}

#[cfg(not(windows))]
mod imp {
    pub(super) fn text() -> Option<String> {
        None
    }

    pub(super) fn set_text(_text: &str) -> bool {
        false
    }
}
