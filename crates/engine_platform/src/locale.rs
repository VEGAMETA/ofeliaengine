//! The user's languages (ADR 0102): what a game's localizer chooses its locale from.

use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};

/// The user's preferred display languages as BCP 47 tags (`ru-RU`, `en-US`), most preferred
/// first, as Windows' language settings order them; empty if Windows does not say.
pub fn preferred_languages() -> Vec<String> {
    let mut count = 0u32;
    let mut len = 0u32;
    // SAFETY: a null buffer asks for the length of the list, written to `len`.
    let sized = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            core::ptr::null_mut(),
            &mut len,
        )
    };
    if sized == 0 || len == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0u16; len as usize];
    // SAFETY: `buffer` holds the `len` characters asked for.
    let read = unsafe {
        GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, buffer.as_mut_ptr(), &mut len)
    };
    if read == 0 {
        return Vec::new();
    }
    // Names separated by nulls, the list ended by another.
    buffer[..(len as usize).min(buffer.len())]
        .split(|&c| c == 0)
        .filter(|name| !name.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_names_languages_as_tags() {
        let languages = preferred_languages();
        // Every Windows installation has a display language.
        assert!(!languages.is_empty());
        for tag in &languages {
            assert!(
                tag.split('-').all(|part| !part.is_empty()
                    && part.chars().all(|c| c.is_ascii_alphanumeric())),
                "{tag}"
            );
        }
    }
}
