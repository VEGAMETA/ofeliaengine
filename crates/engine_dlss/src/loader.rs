//! Finding, verifying and loading the user-supplied Streamline DLLs (CLAUDE.md §6).

use crate::DlssError;
use std::path::{Path, PathBuf};

/// Files DLSS Super Resolution needs from Streamline 2.x (`docs/ProgrammingGuide.md` §4,
/// "Distributing SL with your application"): the interposer, the common plugin, the DLSS
/// plugin, and NVIDIA's DLSS library itself.
pub(crate) const REQUIRED_FILES: [&str; 4] = [
    "sl.interposer.dll",
    "sl.common.dll",
    "sl.dlss.dll",
    "nvngx_dlss.dll",
];

/// Where to look for the DLLs, in order: `explicit`, `ENGINE_STREAMLINE_DIR`, then
/// `plugins/nvidia/` next to the executable, then the executable's folder.
pub(crate) fn candidate_dirs(explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = explicit {
        dirs.push(dir.to_owned());
    }
    if let Some(dir) = std::env::var_os("ENGINE_STREAMLINE_DIR").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_owned))
    {
        dirs.push(exe_dir.join("plugins").join("nvidia"));
        dirs.push(exe_dir);
    }
    dirs
}

/// The first of `dirs` with every [`REQUIRED_FILES`], as an absolute path; otherwise
/// what is missing where the interposer was found, or where it was looked for.
pub(crate) fn find(dirs: &[PathBuf]) -> Result<PathBuf, DlssError> {
    for dir in dirs {
        if !dir.join(REQUIRED_FILES[0]).is_file() {
            continue;
        }
        let missing: Vec<&'static str> = REQUIRED_FILES
            .iter()
            .copied()
            .filter(|f| !dir.join(f).is_file())
            .collect();
        if !missing.is_empty() {
            return Err(DlssError::MissingFiles {
                dir: dir.clone(),
                missing,
            });
        }
        // Streamline requires absolute paths (`sl::Preferences::pathsToPlugins`).
        return std::path::absolute(dir).map_err(|_| DlssError::NotFound {
            searched: dirs.to_vec(),
        });
    }
    Err(DlssError::NotFound {
        searched: dirs.to_vec(),
    })
}

/// A path as a NUL-terminated UTF-16 string.
pub(crate) fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(core::iter::once(0))
        .collect()
}

/// Checks the file's Authenticode signature (`WinVerifyTrust`, as Streamline's
/// `sl::security::verifyEmbeddedSignature` does) and that NVIDIA signed it.
pub(crate) fn verify_signature(file: &Path) -> Result<(), DlssError> {
    use windows_sys::Win32::Security::Cryptography::{
        CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW,
    };
    use windows_sys::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
        WTD_UI_NONE, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
    };

    let path = wide(file);
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: path.as_ptr(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: `data` and what it points to outlive the call; no window handle.
    let status =
        unsafe { WinVerifyTrust(core::ptr::null_mut(), &mut action, (&raw mut data).cast()) };
    let mut signer = String::new();
    if status == 0 {
        // SAFETY: the state data of the verification above, still open.
        let provider = unsafe { WTHelperProvDataFromStateData(data.hWVTStateData) };
        let sgnr = if provider.is_null() {
            core::ptr::null_mut()
        } else {
            // SAFETY: the verification's provider data.
            unsafe { WTHelperGetProvSignerFromChain(provider, 0, 0, 0) }
        };
        // SAFETY: `sgnr` is null or the first signer, with its chain (leaf first).
        if let Some(sgnr) = unsafe { sgnr.as_ref() }
            && sgnr.csCertChain > 0
            && !sgnr.pasCertChain.is_null()
        {
            // SAFETY: the chain has at least one certificate.
            let cert = unsafe { (*sgnr.pasCertChain).pCert };
            let mut name = [0u16; 256];
            // SAFETY: `cert` is the leaf certificate context; the buffer's length is
            // passed.
            let len = unsafe {
                CertGetNameStringW(
                    cert,
                    CERT_NAME_SIMPLE_DISPLAY_TYPE,
                    0,
                    core::ptr::null(),
                    name.as_mut_ptr(),
                    name.len() as u32,
                )
            };
            // The length includes the terminating NUL.
            let len = (len as usize).saturating_sub(1).min(name.len());
            signer = String::from_utf16_lossy(&name[..len]);
        }
    }
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: closes the state the verification opened.
    unsafe {
        WinVerifyTrust(core::ptr::null_mut(), &mut action, (&raw mut data).cast());
    }
    if status != 0 {
        return Err(DlssError::Unsigned {
            file: file.to_owned(),
            reason: format!("WinVerifyTrust returned {:#x}", status as u32),
        });
    }
    if signer != "NVIDIA Corporation" {
        return Err(DlssError::Unsigned {
            file: file.to_owned(),
            reason: format!("signed by \"{signer}\", not NVIDIA Corporation"),
        });
    }
    Ok(())
}

/// A loaded DLL, kept loaded for the rest of the process: Streamline starts threads and
/// hooks that must not outlive its code.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Library(windows_sys::Win32::Foundation::HMODULE);

// SAFETY: a module handle is a process-wide value; the module is never unloaded.
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}

impl Library {
    /// Loads `file`, resolving its own dependencies from its folder.
    pub(crate) fn load(file: &Path) -> Result<Self, DlssError> {
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW,
        };
        let path = wide(file);
        // SAFETY: a NUL-terminated absolute path; loading runs the DLL's initialization,
        // which is NVIDIA's signed code (verified by the caller unless disabled).
        let module = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                core::ptr::null_mut(),
                LOAD_WITH_ALTERED_SEARCH_PATH,
            )
        };
        if module.is_null() {
            return Err(DlssError::Load {
                file: file.to_owned(),
                // SAFETY: no preconditions.
                code: unsafe { windows_sys::Win32::Foundation::GetLastError() },
            });
        }
        Ok(Self(module))
    }

    /// The address of the export `name`.
    pub(crate) fn symbol(&self, name: &'static core::ffi::CStr) -> Result<*const (), DlssError> {
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
        // SAFETY: a loaded module and a NUL-terminated name.
        match unsafe { GetProcAddress(self.0, name.as_ptr().cast()) } {
            Some(f) => Ok(f as *const ()),
            None => Err(DlssError::MissingExport(name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("engine_dlss_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_folders_and_files_are_reported() {
        let empty = scratch("empty");
        assert!(matches!(
            find(std::slice::from_ref(&empty)),
            Err(DlssError::NotFound { .. })
        ));
        let partial = scratch("partial");
        std::fs::write(partial.join("sl.interposer.dll"), b"not a DLL").unwrap();
        std::fs::write(partial.join("sl.dlss.dll"), b"not a DLL").unwrap();
        match find(&[empty.clone(), partial.clone()]) {
            Err(DlssError::MissingFiles { missing, .. }) => {
                assert_eq!(missing, ["sl.common.dll", "nvngx_dlss.dll"]);
            }
            other => panic!("{other:?}"),
        }
        for file in REQUIRED_FILES {
            std::fs::write(partial.join(file), b"not a DLL").unwrap();
        }
        let found = find(&[empty.clone(), partial.clone()]).unwrap();
        assert!(found.is_absolute());
        // An unsigned file is refused before it is loaded; loading it anyway fails cleanly.
        let interposer = found.join("sl.interposer.dll");
        assert!(matches!(
            verify_signature(&interposer),
            Err(DlssError::Unsigned { .. })
        ));
        assert!(matches!(
            Library::load(&interposer),
            Err(DlssError::Load { .. })
        ));
        let _ = std::fs::remove_dir_all(empty);
        let _ = std::fs::remove_dir_all(partial);
    }

    #[test]
    fn a_microsoft_signed_file_is_not_nvidia_signed() {
        // Signed by Microsoft (catalog-signed system files have no embedded signature,
        // so this looks for one with an embedded signature).
        let system =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_default()).join("explorer.exe");
        if !system.is_file() {
            return;
        }
        match verify_signature(&system) {
            Err(DlssError::Unsigned { reason, .. }) => println!("{reason}"),
            other => panic!("{other:?}"),
        }
    }
}
