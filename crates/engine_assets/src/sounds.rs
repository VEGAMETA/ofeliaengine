//! Sounds in packs (ADR 0105; the owner, 2026-10-16: loose files while developing, a pack
//! in a release). A sound pack holds each sound file as it is: no re-encoding, so Ogg
//! Vorbis, Opus, FLAC and WAV keep their codec. Each file is an entry of kind `Sound` (pack
//! version 10) whose meta says what it is:
//! - `meta[0]`: the format ([`SoundFormat`]);
//! - `meta[1]`: the sample rate;
//! - `meta[2]`: the channels;
//! - `meta[3..5]`: the length in frames (low and high words; all ones: not known).
//!
//! One `SoundIndex` entry lists the sounds' paths (with `/`) and their entries.
//!
//! Compressed files are stored raw, so a long sound streams in place from the pack's file
//! ([`SoundPack::region`]). WAV files are compressed with zstd where that saves space,
//! since they are decoded whole anyway.
//!
//! `ofeliaengine sounds build` writes a pack from a folder ([`write_sound_pack`]); the
//! player reads sounds from it by path ([`SoundPack`]).

use crate::pack::{
    Chunk, DEFAULT_CHUNK_SHIFT, Entry, EntryKind, Pack, PackError, PackStats, PackWriter,
    PreparedEntry, invalid,
};
use engine_core::hash::state_hash64;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// What a packed sound's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundFormat {
    /// RIFF/WAVE.
    Wav,
    /// Vorbis in Ogg.
    Vorbis,
    /// Opus in Ogg.
    Opus,
    /// FLAC.
    Flac,
}

impl SoundFormat {
    fn code(self) -> u32 {
        match self {
            Self::Wav => 1,
            Self::Vorbis => 2,
            Self::Opus => 3,
            Self::Flac => 4,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Wav),
            2 => Some(Self::Vorbis),
            3 => Some(Self::Opus),
            4 => Some(Self::Flac),
            _ => None,
        }
    }

    /// The format of a file starting with `head` (its first 128 bytes are enough).
    pub fn sniff(head: &[u8]) -> Option<Self> {
        if head.get(0..4) == Some(b"RIFF") && head.get(8..12) == Some(b"WAVE") {
            return Some(Self::Wav);
        }
        match engine_audio_decode::sniff(head)? {
            engine_audio_decode::Format::Vorbis => Some(Self::Vorbis),
            engine_audio_decode::Format::Opus => Some(Self::Opus),
            engine_audio_decode::Format::Flac => Some(Self::Flac),
        }
    }
}

/// A sound in a pack: where it is and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedSound {
    /// Its entry.
    pub entry: u32,
    /// Its format.
    pub format: SoundFormat,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channels: 1 or 2.
    pub channels: u16,
    /// Length in frames, when known.
    pub frames: Option<u64>,
}

/// What [`write_sound_pack`] found and wrote.
#[derive(Debug, Clone, Default)]
pub struct SoundPackReport {
    /// The pack's entries and bytes.
    pub stats: PackStats,
    /// The sounds packed, by path.
    pub sounds: Vec<(String, PackedSound)>,
    /// Files left out and why (not a sound, damaged, more than two channels).
    pub skipped: Vec<(String, String)>,
}

/// Every sound file under `folder` (its subfolders too) into a pack at `out`, by its path
/// from `folder` with `/`. A file that is not a sound the engine plays is left out and
/// reported. WAV files are compressed with zstd at `level` where that saves space.
///
/// # Errors
///
/// If the folder cannot be read or the pack cannot be written.
pub fn write_sound_pack(
    folder: &Path,
    out: &Path,
    level: i32,
) -> Result<SoundPackReport, PackError> {
    engine_profiler::zone!("assets::write_sound_pack");
    let mut files = Vec::new();
    gather(folder, folder, &mut files)?;
    files.sort();
    let mut report = SoundPackReport::default();
    let mut writer = PackWriter::create(out, DEFAULT_CHUNK_SHIFT)?;
    for (name, path) in &files {
        let bytes = std::fs::read(path)?;
        let sound = match measure(&bytes) {
            Ok(sound) => sound,
            Err(why) => {
                report.skipped.push((name.clone(), why));
                continue;
            }
        };
        let frames = sound.frames.unwrap_or(u64::MAX);
        let meta = [
            sound.format.code(),
            sound.sample_rate,
            u32::from(sound.channels),
            frames as u32,
            (frames >> 32) as u32,
            0,
            0,
        ];
        // Compressed audio stays raw: it streams in place.
        let level = if sound.format == SoundFormat::Wav {
            level
        } else {
            0
        };
        let entry = writer.add(&PreparedEntry::new(
            EntryKind::Sound,
            meta,
            &bytes,
            level,
            DEFAULT_CHUNK_SHIFT,
        ))?;
        report
            .sounds
            .push((name.clone(), PackedSound { entry, ..sound }));
    }
    let index = index_bytes(&report.sounds);
    writer.add(&PreparedEntry::new(
        EntryKind::SoundIndex,
        [report.sounds.len() as u32, 0, 0, 0, 0, 0, 0],
        &index,
        level.max(1),
        DEFAULT_CHUNK_SHIFT,
    ))?;
    report.stats = writer.finish()?;
    Ok(report)
}

/// The files under `dir`, by their path from `root` with `/`.
fn gather(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            gather(root, &path, out)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            let name = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((name, path));
        }
    }
    Ok(())
}

/// What a sound file is (its entry still to be given).
fn measure(bytes: &[u8]) -> Result<PackedSound, String> {
    let head = &bytes[..bytes.len().min(128)];
    let format = SoundFormat::sniff(head).ok_or("not a WAV, Ogg Vorbis, Opus or FLAC file")?;
    let (sample_rate, channels, frames) = if format == SoundFormat::Wav {
        let clip = engine_audio::Clip::from_wav(bytes).map_err(|e| e.to_string())?;
        (
            clip.sample_rate(),
            clip.channels(),
            Some(clip.frames() as u64),
        )
    } else {
        let mut decoder =
            engine_audio_decode::open_bytes(bytes.to_vec()).map_err(|e| e.to_string())?;
        // Ogg files do not say their length: it is counted once here, so the player knows a
        // short sound from a long one without decoding it.
        let frames = match decoder.frames() {
            Some(frames) => frames,
            None => {
                let mut scratch = vec![0.0f32; 4096 * usize::from(decoder.channels().max(1))];
                let mut frames = 0u64;
                loop {
                    match decoder.read(&mut scratch).map_err(|e| e.to_string())? {
                        0 => break frames,
                        n => frames += n as u64,
                    }
                }
            }
        };
        (decoder.sample_rate(), decoder.channels(), Some(frames))
    };
    Ok(PackedSound {
        entry: 0,
        format,
        sample_rate,
        channels,
        frames,
    })
}

/// The index: a count, then per sound its entry, its path's length and its path.
fn index_bytes(sounds: &[(String, PackedSound)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(sounds.len() as u32).to_le_bytes());
    for (path, sound) in sounds {
        out.extend_from_slice(&sound.entry.to_le_bytes());
        out.extend_from_slice(&(path.len() as u32).to_le_bytes());
        out.extend_from_slice(path.as_bytes());
    }
    out
}

/// An open sound pack: its sounds by path.
#[derive(Debug)]
pub struct SoundPack {
    path: PathBuf,
    pack: Pack,
    sounds: HashMap<String, PackedSound>,
}

impl SoundPack {
    /// Opens the sound pack at `path` and reads its index.
    ///
    /// # Errors
    ///
    /// If it is not a pack, has no sound index, or the index or an entry it names is
    /// damaged.
    pub fn open(path: &Path) -> Result<Self, PackError> {
        engine_profiler::zone!("assets::SoundPack::open");
        let pack = Pack::open(path)?;
        let mut file = std::fs::File::open(path)?;
        let (_, index) = pack
            .entries()
            .iter()
            .enumerate()
            .find(|(_, e)| e.kind() == EntryKind::SoundIndex)
            .ok_or_else(|| invalid("no sound index"))?;
        let bytes = read_entry(&mut file, &pack, index)?;
        let mut sounds = HashMap::new();
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8], PackError> {
            let slice = bytes
                .get(at..at + n)
                .ok_or_else(|| invalid("damaged sound index"))?;
            at += n;
            Ok(slice)
        };
        let word = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let count = word(take(4)?);
        for _ in 0..count {
            let entry = word(take(4)?);
            let length = word(take(4)?) as usize;
            let name = std::str::from_utf8(take(length)?)
                .map_err(|_| invalid("a sound's path is not UTF-8"))?
                .to_owned();
            let e = pack
                .entries()
                .get(entry as usize)
                .filter(|e| e.kind() == EntryKind::Sound)
                .ok_or_else(|| invalid(format!("{name}: no sound entry {entry}")))?;
            let frames = u64::from(e.meta[3]) | (u64::from(e.meta[4]) << 32);
            let sound = PackedSound {
                entry,
                format: SoundFormat::from_code(e.meta[0])
                    .ok_or_else(|| invalid(format!("{name}: unknown sound format")))?,
                sample_rate: e.meta[1],
                channels: e.meta[2] as u16,
                frames: (frames != u64::MAX).then_some(frames),
            };
            sounds.insert(name, sound);
        }
        Ok(Self {
            path: path.to_owned(),
            pack,
            sounds,
        })
    }

    /// The sound at `path` (with `/`), if the pack has it.
    pub fn get(&self, path: &str) -> Option<&PackedSound> {
        self.sounds.get(path)
    }

    /// The sounds' paths.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.sounds.keys().map(String::as_str)
    }

    /// Its number of sounds.
    pub fn len(&self) -> usize {
        self.sounds.len()
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.sounds.is_empty()
    }

    /// A sound's whole file, read and checked.
    ///
    /// # Errors
    ///
    /// If the pack cannot be read or the entry is damaged.
    pub fn read(&self, sound: &PackedSound) -> Result<Vec<u8>, PackError> {
        let entry = self.entry(sound)?;
        let mut file = std::fs::File::open(&self.path)?;
        read_entry(&mut file, &self.pack, entry)
    }

    /// A sound's file in place in the pack, to stream from: `None` when it is stored
    /// compressed (a WAV file; [`Self::read`] it).
    ///
    /// # Errors
    ///
    /// If the pack cannot be opened again.
    pub fn region(&self, sound: &PackedSound) -> Result<Option<SoundRegion>, PackError> {
        let entry = self.entry(sound)?;
        if !entry.is_raw() {
            return Ok(None);
        }
        let mut file = std::fs::File::open(&self.path)?;
        file.seek(SeekFrom::Start(entry.offset))?;
        Ok(Some(SoundRegion {
            file: io::BufReader::with_capacity(64 * 1024, file),
            start: entry.offset,
            len: entry.bytes,
            at: 0,
        }))
    }

    fn entry(&self, sound: &PackedSound) -> Result<&Entry, PackError> {
        self.pack
            .entries()
            .get(sound.entry as usize)
            .filter(|e| e.kind() == EntryKind::Sound)
            .ok_or_else(|| invalid(format!("no sound entry {}", sound.entry)))
    }
}

/// An entry's raw bytes, its chunks decompressed and checked against their hashes.
fn read_entry(file: &mut std::fs::File, pack: &Pack, entry: &Entry) -> Result<Vec<u8>, PackError> {
    let mut stored = vec![0u8; entry.stored_bytes as usize];
    file.seek(SeekFrom::Start(entry.offset))?;
    file.read_exact(&mut stored)?;
    let mut out = Vec::with_capacity(entry.bytes as usize);
    let chunk_bytes = 1u64 << pack.chunk_shift().min(30);
    for chunk in pack.chunks(entry) {
        let Chunk { offset, .. } = *chunk;
        let at = offset as usize;
        let bytes = stored
            .get(at..at + chunk.stored_bytes() as usize)
            .ok_or_else(|| invalid("a chunk outside its entry"))?;
        let start = out.len();
        if chunk.is_raw() {
            out.extend_from_slice(bytes);
        } else {
            // This chunk's raw length: a whole chunk, or what is left of the entry.
            let length = chunk_bytes.min(entry.bytes - start as u64) as usize;
            let raw = zstd::bulk::decompress(bytes, length)
                .map_err(|e| invalid(format!("a damaged chunk: {e}")))?;
            out.extend_from_slice(&raw);
        }
        if state_hash64(&out[start..]) != chunk.hash {
            return Err(invalid("a damaged chunk"));
        }
    }
    if out.len() as u64 != entry.bytes {
        return Err(invalid("an entry of the wrong length"));
    }
    Ok(out)
}

/// A sound's bytes in place in its pack: a file of its own to the decoders.
#[derive(Debug)]
pub struct SoundRegion {
    file: io::BufReader<std::fs::File>,
    start: u64,
    len: u64,
    at: u64,
}

impl Read for SoundRegion {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.len - self.at;
        if left == 0 {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(left) as usize;
        let n = self.file.read(&mut buf[..n])?;
        self.at += n as u64;
        Ok(n)
    }
}

impl Seek for SoundRegion {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(n) => i128::from(self.len) + i128::from(n),
            SeekFrom::Current(n) => i128::from(self.at) + i128::from(n),
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "before the start",
            ));
        }
        let target = (target as u64).min(self.len);
        self.file.seek(SeekFrom::Start(self.start + target))?;
        self.at = target;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine_audio_decode/tests/data")
    }

    #[test]
    fn a_folder_of_sounds_round_trips_through_a_pack() {
        let dir = std::env::temp_dir().join(format!("engine_sound_pack_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = dir.join("sounds");
        std::fs::create_dir_all(folder.join("music")).unwrap();
        for name in [
            "stereo_44100.ogg",
            "stereo_48000.opus",
            "stereo_44100_s16.flac",
            "stereo_44100_s16.wav",
        ] {
            std::fs::copy(fixtures().join(name), folder.join(name)).unwrap();
        }
        std::fs::copy(
            fixtures().join("mono_22050.ogg"),
            folder.join("music/theme.ogg"),
        )
        .unwrap();
        std::fs::write(folder.join("notes.txt"), b"not a sound").unwrap();
        let out = dir.join("sounds.pack");
        let report = write_sound_pack(&folder, &out, 19).unwrap();
        assert_eq!(report.sounds.len(), 5);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "notes.txt");

        let pack = SoundPack::open(&out).unwrap();
        assert_eq!(pack.len(), 5);
        let file = std::fs::read(&out).unwrap();
        let header: crate::pack::Header =
            bytemuck::pod_read_unaligned(&file[..size_of::<crate::pack::Header>()]);
        assert_eq!(header.version, 10);
        let theme = *pack.get("music/theme.ogg").unwrap();
        assert_eq!(
            (theme.format, theme.sample_rate, theme.channels),
            (SoundFormat::Vorbis, 22_050, 1)
        );
        // Whole, the same bytes; in place, a stream the decoders read as the file.
        assert_eq!(
            pack.read(&theme).unwrap(),
            std::fs::read(folder.join("music/theme.ogg")).unwrap()
        );
        let region = pack.region(&theme).unwrap().unwrap();
        let mut from_pack = engine_audio_decode::open(region).unwrap();
        let mut from_file = engine_audio_decode::open_file(folder.join("music/theme.ogg")).unwrap();
        assert!(theme.frames.is_some_and(|f| f > 0));
        let (mut a, mut b) = (vec![0.0f32; 4096], vec![0.0f32; 4096]);
        loop {
            let (n, m) = (
                from_pack.read(&mut a).unwrap(),
                from_file.read(&mut b).unwrap(),
            );
            assert_eq!(n, m);
            if n == 0 {
                break;
            }
            assert_eq!(a[..n], b[..m]);
        }
        // A sought region: the decoders seek through it.
        let mut region = pack.region(&theme).unwrap().unwrap();
        region.seek(SeekFrom::Start(10)).unwrap();
        let mut ten = [0u8; 4];
        region.read_exact(&mut ten).unwrap();
        assert_eq!(
            &ten,
            &file[pack.entry(&theme).unwrap().offset as usize + 10..][..4]
        );
        drop(pack);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
