//! Localized text in packs (ADR 0102): an entry per locale (kind `Strings`, pack version 7),
//! the compiled `engine_loc::Strings` table as bytes; the source locale's (the one every
//! other falls back to) marked in its `meta[0]`, its message count in `meta[1]`.
//!
//! `ofeliaengine loc build` writes a strings pack of its own ([`write_strings_pack`]); the
//! player reads one into a localizer ([`load_localizer`]).

use crate::pack::{
    DEFAULT_CHUNK_SHIFT, EntryKind, Pack, PackError, PackStats, PackWriter, PreparedEntry, invalid,
};
use engine_core::jobs::ThreadPool;
use engine_loc::{Localizer, Strings};
use std::path::Path;
use std::sync::Mutex;

/// `meta[0]` of the source locale's entry.
const SOURCE: u32 = 1;

/// Writes `tables` to a pack at `path`, one `Strings` entry each, `source`'s marked as the
/// source locale's; compressed with zstd at `level` where that saves space.
///
/// # Errors
///
/// If no table has the locale `source`, two share a locale, or the file cannot be written.
pub fn write_strings_pack(
    path: &Path,
    tables: &[Strings],
    source: &str,
    level: i32,
) -> Result<PackStats, PackError> {
    engine_profiler::zone!("assets::write_strings_pack");
    if !tables.iter().any(|t| t.locale == source) {
        return Err(invalid(format!("no table of the source locale `{source}`")));
    }
    for (k, t) in tables.iter().enumerate() {
        if tables[..k].iter().any(|o| o.locale == t.locale) {
            return Err(invalid(format!("two tables of `{}`", t.locale)));
        }
    }
    let mut writer = PackWriter::create(path, DEFAULT_CHUNK_SHIFT)?;
    // The source locale's first: it is read whatever the player's language.
    let mut order: Vec<&Strings> = tables.iter().collect();
    order.sort_by_key(|t| t.locale != source);
    for t in order {
        let meta = [
            if t.locale == source { SOURCE } else { 0 },
            t.messages() as u32,
            0,
            0,
            0,
            0,
            0,
        ];
        let entry = PreparedEntry::new(
            EntryKind::Strings,
            meta,
            &t.to_bytes(),
            level,
            DEFAULT_CHUNK_SHIFT,
        );
        writer.add(&entry)?;
    }
    writer.finish()
}

/// The tables of `pack`'s `Strings` entries (in the pack's order, the source locale's
/// first) and the source locale, decompressed on `jobs`.
///
/// # Errors
///
/// If an entry cannot be read or is not a table, or the pack has no source locale (or two).
pub fn load_strings(pack: &Pack, jobs: &ThreadPool) -> Result<(Vec<Strings>, String), PackError> {
    engine_profiler::zone!("assets::load_strings");
    let entries = pack.entries();
    let indices: Vec<u32> = (0..entries.len() as u32)
        .filter(|&i| entries[i as usize].kind() == EntryKind::Strings)
        .collect();
    let slots: Vec<Mutex<Option<Strings>>> = indices.iter().map(|_| Mutex::new(None)).collect();
    let position = |index: u32| indices.binary_search(&index).unwrap_or(usize::MAX);
    pack.load(
        &indices,
        jobs,
        indices.len().clamp(1, 8),
        |index, _, data| {
            let table =
                Strings::from_bytes(&data).ok_or_else(|| invalid("damaged strings entry"))?;
            if let Some(slot) = slots.get(position(index)) {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(table);
            }
            Ok(())
        },
    )?;
    let mut source = None;
    let mut tables = Vec::with_capacity(indices.len());
    for (slot, &index) in slots.into_iter().zip(&indices) {
        let table = slot
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
            .ok_or_else(|| invalid("a strings entry was not loaded"))?;
        if entries[index as usize].meta[0] == SOURCE
            && source.replace(table.locale.clone()).is_some()
        {
            return Err(invalid("two source locales"));
        }
        tables.push(table);
    }
    let source = source.ok_or_else(|| invalid("no source locale's text"))?;
    Ok((tables, source))
}

/// A localizer of the strings pack at `path`, its locale the first of `preferred` (BCP 47
/// tags, the player's languages in order) that it serves, else its source locale.
///
/// # Errors
///
/// If the pack cannot be read, holds no valid tables, or a table's locale has no rules.
pub fn load_localizer(
    path: &Path,
    jobs: &ThreadPool,
    preferred: &[&str],
) -> Result<Localizer, PackError> {
    let pack = Pack::open(path)?;
    let (tables, source) = load_strings(&pack, jobs)?;
    let mut localizer = Localizer::new(tables, &source).map_err(|e| invalid(e.to_string()))?;
    localizer.set_locale(preferred);
    Ok(localizer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_loc::{Args, loc};

    fn tables() -> Vec<Strings> {
        let en = "items = { $count ->
    [one] { $count } item
   *[other] { $count } items
}
";
        let ru = "items = { $count ->
    [one] { $count } предмет
    [few] { $count } предмета
   *[many] { $count } предметов
}
";
        vec![
            engine_loc::compile("en", &[("en.ftl", en)]).0,
            engine_loc::compile("ru", &[("ru.ftl", ru)]).0,
        ]
    }

    fn jobs() -> ThreadPool {
        ThreadPool::builder().workers(2).build().unwrap()
    }

    #[test]
    fn tables_round_trip_through_a_pack() {
        let dir = std::env::temp_dir().join(format!("engine_strings_pack_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("strings.pack");
        let tables = tables();
        // The source given last: written first.
        let reversed: Vec<Strings> = tables.iter().rev().cloned().collect();
        let stats = write_strings_pack(&path, &reversed, "en", 19).unwrap();
        assert_eq!(stats.entries, 2);
        let pack = Pack::open(&path).unwrap();
        let file = std::fs::read(&path).unwrap();
        let header: crate::pack::Header =
            bytemuck::pod_read_unaligned(&file[..size_of::<crate::pack::Header>()]);
        assert_eq!(header.version, 7);
        let (read, source) = load_strings(&pack, &jobs()).unwrap();
        assert_eq!(source, "en");
        assert_eq!(read, tables);
        let mut l = load_localizer(&path, &jobs(), &["ru-RU", "en-US"]).unwrap();
        assert_eq!(l.locale(), "ru");
        assert_eq!(
            l.text(loc!("items"), &Args::new().with("count", 21)),
            "21 предмет"
        );
        l.set_locale(&["de"]);
        assert_eq!(
            l.text(loc!("items"), &Args::new().with("count", 21)),
            "21 items"
        );
        drop(pack);
        let _ = std::fs::remove_dir_all(&dir);
        // A pack without its source locale's table is refused when written.
        assert!(write_strings_pack(&dir.join("x.pack"), &tables[1..], "en", 3).is_err());
    }
}
