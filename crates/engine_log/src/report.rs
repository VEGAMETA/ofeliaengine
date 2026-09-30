//! Report sections: context that subsystems add to crash reports (GPU markers, the scene
//! being loaded), and the directory reports go to.
//!
//! A crash report ([`write_sections`]) must not wait: it takes the section list with
//! `try_lock` and says so if another thread holds it.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type SectionFn = dyn Fn(&mut dyn fmt::Write) -> fmt::Result + Send + Sync;

struct Section {
    id: u64,
    name: &'static str,
    write: Arc<SectionFn>,
}

static SECTIONS: Mutex<Vec<Section>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Keeps a section in crash reports; dropping it removes the section.
#[must_use = "the section is removed when the guard is dropped"]
#[derive(Debug)]
pub struct SectionGuard {
    id: u64,
}

impl Drop for SectionGuard {
    fn drop(&mut self) {
        if let Ok(mut sections) = SECTIONS.lock() {
            sections.retain(|s| s.id != self.id);
        }
    }
}

/// Adds a section to crash reports: `write` is called when a report is written, possibly
/// on a crashing thread, so it should only read memory it owns, without allocating or
/// taking locks that the crashed code may hold.
pub fn add_section(
    name: &'static str,
    write: impl Fn(&mut dyn fmt::Write) -> fmt::Result + Send + Sync + 'static,
) -> SectionGuard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut sections) = SECTIONS.lock() {
        sections.push(Section {
            id,
            name,
            write: Arc::new(write),
        });
    }
    SectionGuard { id }
}

/// Writes every section, each under a `== name ==` heading. Never blocks.
pub fn write_sections(out: &mut dyn fmt::Write) -> fmt::Result {
    let sections = match SECTIONS.try_lock() {
        Ok(sections) => sections,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return writeln!(out, "(report sections are being changed by another thread)");
        }
    };
    for section in sections.iter() {
        writeln!(out, "== {} ==", section.name)?;
        (section.write)(out)?;
        writeln!(out)?;
    }
    Ok(())
}

/// Sets the directory crash reports and GPU fault dumps are written to.
pub fn set_dir(dir: impl Into<PathBuf>) {
    if let Ok(mut d) = DIR.lock() {
        *d = Some(dir.into());
    }
}

/// The directory crash reports and GPU fault dumps go to: the one given to [`set_dir`],
/// else `%LOCALAPPDATA%\ofeliaengine\reports` (the temporary directory without it).
pub fn dir() -> PathBuf {
    if let Ok(d) = DIR.lock()
        && let Some(dir) = d.as_ref()
    {
        return dir.clone();
    }
    data_dir().join("reports")
}

/// Where an application's log file goes by default:
/// `%LOCALAPPDATA%\ofeliaengine\logs\<app>.log`.
pub fn log_file(app: &str) -> PathBuf {
    data_dir().join("logs").join(format!("{app}.log"))
}

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ofeliaengine")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_come_and_go() {
        let a = add_section("alpha", |out| writeln!(out, "first"));
        let b = add_section("beta", |out| writeln!(out, "second"));
        let mut text = String::new();
        write_sections(&mut text).unwrap();
        assert!(text.contains("== alpha ==\nfirst\n"), "{text}");
        assert!(text.contains("== beta ==\nsecond\n"), "{text}");
        drop(a);
        text.clear();
        write_sections(&mut text).unwrap();
        assert!(!text.contains("alpha"), "{text}");
        drop(b);
    }
}
