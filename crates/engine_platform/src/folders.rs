//! Windows' known folders: where a game's saves go (ADR 0104).

use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

/// The user's `Saved Games` folder (`FOLDERID_SavedGames`, `%USERPROFILE%\Saved Games`
/// unless the user moved it), or `None` when Windows cannot say.
pub fn saved_games() -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_SavedGames, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };
    let mut path: windows_sys::core::PWSTR = core::ptr::null_mut();
    // SAFETY: a known folder's id, no token (the current user), an out pointer; Windows
    // allocates the string, freed below with `CoTaskMemFree` whatever the result.
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_SavedGames,
            KF_FLAG_DEFAULT as u32,
            core::ptr::null_mut(),
            &mut path,
        )
    };
    let found = (hr >= 0 && !path.is_null()).then(|| {
        // SAFETY: on success the string is NUL-terminated UTF-16 that Windows owns until
        // freed.
        let len = unsafe { (0..).take_while(|&i| *path.add(i) != 0).count() };
        // SAFETY: `len` UTF-16 units of it, read before it is freed.
        let units = unsafe { core::slice::from_raw_parts(path, len) };
        PathBuf::from(std::ffi::OsString::from_wide(units))
    });
    // SAFETY: allocated by `SHGetKnownFolderPath` (or null, which it accepts).
    unsafe { CoTaskMemFree(path.cast()) };
    found
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_saved_games_folder_is_found() {
        let dir = super::saved_games().expect("Windows names the folder");
        assert!(dir.is_absolute(), "{}", dir.display());
    }
}
