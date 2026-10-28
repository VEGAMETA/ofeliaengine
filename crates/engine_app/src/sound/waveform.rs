//! A sound file's waveform, for the editor's thumbnails and inspector (ADR 0107 E11-3,
//! `docs/research/ux/sound.md`): read once, whole, on the caller's thread.

use engine_assets::sounds::SoundFormat;
use engine_audio::{Clip, Decoder};
use std::path::Path;

/// Frames a block of a compressed file is summed over while it decodes: its length is
/// known only at the end (Ogg files do not say it), so blocks are made, then joined into
/// columns.
const BLOCK: usize = 256;
/// Frames decoded at a time.
const CHUNK: usize = 4096;
/// A sample this quiet or quieter is silence for a waveform's facts: −60 dBFS.
const SILENCE: f32 = 0.001;
/// A column before any sample: its lowest above its highest.
const EMPTY: [f32; 2] = [f32::INFINITY, f32::NEG_INFINITY];

/// A sound's waveform: each column's lowest and highest sample over its part of the file,
/// every channel together and each by itself, and what the file is.
#[derive(Debug, Clone, PartialEq)]
pub struct Waveform {
    /// Each column's lowest and highest sample, from the file's start to its end.
    pub columns: Vec<[f32; 2]>,
    /// The same for each channel by itself (one list for a mono file): the inspector's
    /// bands, a channel each.
    pub channel_columns: Vec<Vec<[f32; 2]>>,
    /// Frames a second.
    pub sample_rate: u32,
    /// 1 or 2.
    pub channels: u16,
    /// Its length in frames.
    pub frames: u64,
    /// Its loudest sample's level, from 0 (silence) to 1 (full scale) and over.
    pub peak: f32,
    /// Frames of silence (every channel at −60 dBFS or below) at its start and at its end;
    /// both its whole length for a silent file.
    pub silence: [u64; 2],
}

impl Waveform {
    /// Its length in seconds.
    pub fn seconds(&self) -> f64 {
        self.seconds_of(self.frames)
    }

    /// `frames` of it in seconds.
    pub fn seconds_of(&self, frames: u64) -> f64 {
        frames as f64 / f64::from(self.sample_rate.max(1))
    }

    /// Its peak in dBFS: 0 at full scale, below it negative (minus infinity for silence).
    pub fn peak_dbfs(&self) -> f32 {
        20.0 * self.peak.log10()
    }

    /// Nothing in it is heard: every sample at −60 dBFS or below.
    pub fn is_silent(&self) -> bool {
        self.peak <= SILENCE
    }
}

/// Columns `a` and `b` as one.
fn join(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0].min(b[0]), a[1].max(b[1])]
}

/// The waveform of the sound file at `file` (WAV, Ogg Vorbis or Opus, FLAC) in `columns`
/// columns, read whole: what the editor's thumbnail thread asks.
///
/// # Errors
///
/// When the file cannot be read or is not a sound the engine reads.
pub fn waveform(file: &Path, columns: usize) -> Result<Waveform, String> {
    engine_profiler::zone!("app::sound::waveform");
    let why = |e: &dyn core::fmt::Display| format!("{}: {e}", file.display());
    let columns = columns.max(1);
    let mut head = [0u8; 12];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(file).map_err(|e| why(&e))?;
        let mut filled = 0;
        while filled < head.len() {
            match f.read(&mut head[filled..]).map_err(|e| why(&e))? {
                0 => break,
                n => filled += n,
            }
        }
    }
    if SoundFormat::sniff(&head) == Some(SoundFormat::Wav) {
        let bytes = std::fs::read(file).map_err(|e| why(&e))?;
        let clip = Clip::from_wav(&bytes).map_err(|e| why(&e))?;
        return Ok(of_clip(&clip, columns));
    }
    let mut decoder = engine_audio_decode::open_file(file).map_err(|e| why(&e))?;
    of_decoder(decoder.as_mut(), columns).map_err(|e| why(&e))
}

/// A clip's waveform.
fn of_clip(clip: &Clip, columns: usize) -> Waveform {
    let frames = clip.frames();
    let channels = usize::from(clip.channels().max(1));
    let mut mixed = vec![[0.0f32; 2]; columns];
    let mut each = vec![vec![[0.0f32; 2]; columns]; channels];
    let mut peak = 0.0f32;
    for (k, column) in mixed.iter_mut().enumerate() {
        let (start, end) = span(k, columns, frames);
        if start == end {
            continue;
        }
        let mut all = EMPTY;
        for (c, list) in each.iter_mut().enumerate() {
            let low_high = clip.channel(c)[start..end]
                .iter()
                .fold(EMPTY, |a, &s| join(a, [s, s]));
            list[k] = low_high;
            all = join(all, low_high);
        }
        *column = all;
        peak = peak.max(all[1]).max(-all[0]);
    }
    // The first and the last frame any channel is heard in.
    let heard = |s: &f32| s.abs() > SILENCE;
    let first = (0..channels)
        .filter_map(|c| clip.channel(c).iter().position(heard))
        .min();
    let last = (0..channels)
        .filter_map(|c| clip.channel(c).iter().rposition(heard))
        .max();
    let silence = match (first, last) {
        (Some(first), Some(last)) => [first as u64, (frames - 1 - last) as u64],
        _ => [frames as u64; 2],
    };
    Waveform {
        columns: mixed,
        channel_columns: each,
        sample_rate: clip.sample_rate(),
        channels: clip.channels(),
        frames: frames as u64,
        peak,
        silence,
    }
}

/// A decoder's waveform, decoded to its end.
fn of_decoder(
    decoder: &mut dyn Decoder,
    columns: usize,
) -> Result<Waveform, engine_audio::DecodeError> {
    let channels = usize::from(decoder.channels().max(1));
    let mut buffer = vec![0.0f32; CHUNK * channels];
    // Each block's lowest and highest sample of each channel (a block's channels side by
    // side), and the block being filled.
    let mut blocks: Vec<[f32; 2]> = Vec::new();
    let mut current = vec![EMPTY; channels];
    let mut in_block = 0;
    let mut frames = 0u64;
    // The first and the last frame any channel is heard in.
    let mut heard: Option<(u64, u64)> = None;
    loop {
        let read = decoder.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for (i, frame) in buffer[..read * channels].chunks_exact(channels).enumerate() {
            let mut loud = false;
            for (low_high, &s) in current.iter_mut().zip(frame) {
                *low_high = join(*low_high, [s, s]);
                loud |= s.abs() > SILENCE;
            }
            if loud {
                let at = frames + i as u64;
                heard = Some(heard.map_or((at, at), |(first, _)| (first, at)));
            }
            in_block += 1;
            if in_block == BLOCK {
                blocks.extend_from_slice(&current);
                current.fill(EMPTY);
                in_block = 0;
            }
        }
        frames += read as u64;
    }
    if in_block > 0 {
        blocks.extend_from_slice(&current);
    }
    let count = blocks.len() / channels;
    let mut mixed = vec![[0.0f32; 2]; columns];
    let mut each = vec![vec![[0.0f32; 2]; columns]; channels];
    let mut peak = 0.0f32;
    for (k, column) in mixed.iter_mut().enumerate() {
        let (start, end) = span(k, columns, count);
        if start == end {
            continue;
        }
        let mut all = EMPTY;
        for (c, list) in each.iter_mut().enumerate() {
            let low_high = blocks[start * channels..end * channels]
                .iter()
                .skip(c)
                .step_by(channels)
                .fold(EMPTY, |a, &b| join(a, b));
            list[k] = low_high;
            all = join(all, low_high);
        }
        *column = all;
        peak = peak.max(all[1]).max(-all[0]);
    }
    let silence = heard.map_or([frames; 2], |(first, last)| [first, frames - 1 - last]);
    Ok(Waveform {
        columns: mixed,
        channel_columns: each,
        sample_rate: decoder.sample_rate(),
        channels: decoder.channels(),
        frames,
        peak,
        silence,
    })
}

/// Column `k`'s part of `len` items in `columns` columns: every item in exactly one column;
/// a column of a short sound repeats its item rather than stay empty.
fn span(k: usize, columns: usize, len: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let start = (k * len / columns).min(len - 1);
    let end = ((k + 1) * len / columns).clamp(start + 1, len);
    (start, end)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A WAV file of `samples` (mono, 16-bit) at 48 kHz.
    fn wav(samples: &[f32]) -> Vec<u8> {
        let data = samples.len() * 2;
        let mut out = Vec::with_capacity(44 + data);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&u32::try_from(36 + data).unwrap().to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&48_000u32.to_le_bytes());
        out.extend_from_slice(&96_000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&u32::try_from(data).unwrap().to_le_bytes());
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    #[test]
    fn a_waveform_holds_each_columns_lowest_and_highest_sample() {
        // A second: silence, then a tone at half scale, then silence.
        let samples: Vec<f32> = (0..48_000)
            .map(|i| {
                if (16_000..32_000).contains(&i) {
                    0.5 * (i as f32 * 0.05).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let dir = std::env::temp_dir().join(format!("ofelia-waveform-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tone.wav");
        std::fs::write(&file, wav(&samples)).unwrap();
        let w = waveform(&file, 12).unwrap();
        assert_eq!((w.frames, w.sample_rate, w.channels), (48_000, 48_000, 1));
        assert!((w.seconds() - 1.0).abs() < 1e-9);
        assert!((w.peak - 0.5).abs() < 1e-3, "{}", w.peak);
        // The first and last thirds are flat, the middle one swings to ±0.5.
        for (k, column) in w.columns.iter().enumerate() {
            let swing = column[1] - column[0];
            if (4..8).contains(&k) {
                assert!(swing > 0.9, "column {k}: {column:?}");
            } else {
                assert!(swing.abs() < 1e-3, "column {k}: {column:?}");
            }
        }
        // Its silence: the first and last thirds; its peak in dBFS.
        assert_eq!(w.silence, [16_000, 16_000]);
        assert!((w.peak_dbfs() - (-6.02)).abs() < 0.05, "{}", w.peak_dbfs());
        assert_eq!(
            w.channel_columns,
            std::slice::from_ref(&w.columns),
            "mono: one band, the same"
        );
        // More columns than frames: each column shows a frame.
        std::fs::write(&file, wav(&[0.25, -0.25, 0.5])).unwrap();
        let w = waveform(&file, 6).unwrap();
        assert!(w.columns.iter().all(|c| c[0] <= c[1]));
        assert!((w.peak - 0.5).abs() < 1e-3);
        assert_eq!(w.silence, [0, 0]);
        // A silent file is silent from end to end.
        std::fs::write(&file, wav(&[0.0; 480])).unwrap();
        let w = waveform(&file, 6).unwrap();
        assert_eq!(w.silence, [480, 480]);
        assert_eq!(w.peak_dbfs(), f32::NEG_INFINITY);
        assert!(w.is_silent());
        // Not a sound.
        std::fs::write(&file, b"not a sound at all").unwrap();
        assert!(waveform(&file, 6).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stereo_files_channels_have_their_own_bands() {
        // Left: a tone in the second half; right: silence; 16-bit at 48 kHz.
        let frames = 4_800u32;
        let data = frames * 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&192_000u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data.to_le_bytes());
        for i in 0..frames {
            let left = if i >= frames / 2 {
                (0.8 * (i as f32 * 0.1).sin() * 32767.0) as i16
            } else {
                0
            };
            bytes.extend_from_slice(&left.to_le_bytes());
            bytes.extend_from_slice(&0i16.to_le_bytes());
        }
        let dir = std::env::temp_dir().join(format!("ofelia-waveform-st-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("left.wav");
        std::fs::write(&file, bytes).unwrap();
        let w = waveform(&file, 8).unwrap();
        assert_eq!((w.channels, w.channel_columns.len()), (2, 2));
        let swing = |c: [f32; 2]| c[1] - c[0];
        for k in 0..8 {
            let (left, right) = (w.channel_columns[0][k], w.channel_columns[1][k]);
            assert!(swing(right).abs() < 1e-3, "right {k}: {right:?}");
            if k >= 4 {
                assert!(swing(left) > 1.2, "left {k}: {left:?}");
            } else {
                assert!(swing(left).abs() < 1e-3, "left {k}: {left:?}");
            }
            assert_eq!(w.columns[k], join(left, right), "together: both");
        }
        // Silent until the tone: its first half, and none at its end.
        assert_eq!(w.silence[1], 0);
        assert!(
            w.silence[0] >= 2_400 && w.silence[0] < 2_410,
            "{:?}",
            w.silence
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The editor's thumbnail cost (`ENGINE_WAVEFORM_FILES="a.ogg;b.wav" cargo test --release
    /// -p engine_app --lib -- --ignored --nocapture measure_waveforms`): each file's
    /// waveform in 128 columns, the fastest of three reads.
    #[test]
    #[ignore = "a measurement"]
    fn measure_waveforms() {
        let files = std::env::var("ENGINE_WAVEFORM_FILES").unwrap_or_default();
        for file in files.split(';').filter(|f| !f.is_empty()) {
            let mut best = f64::INFINITY;
            let mut seconds = 0.0;
            for _ in 0..3 {
                let t = std::time::Instant::now();
                let w = waveform(Path::new(file), 128).unwrap();
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
                seconds = w.seconds();
            }
            println!("{file}: {seconds:.1} s of sound, its waveform in {best:.1} ms");
        }
    }

    #[test]
    fn a_decoders_waveform_is_joined_from_blocks_whatever_its_length() {
        // A decoder that does not say its length (as Ogg files do not).
        struct Ramp {
            left: u64,
            at: u64,
        }
        impl Decoder for Ramp {
            fn sample_rate(&self) -> u32 {
                48_000
            }
            fn channels(&self) -> u16 {
                2
            }
            fn frames(&self) -> Option<u64> {
                None
            }
            fn read(&mut self, out: &mut [f32]) -> Result<usize, engine_audio::DecodeError> {
                let n = (out.len() as u64 / 2).min(self.left) as usize;
                for frame in out[..n * 2].chunks_exact_mut(2) {
                    let v = self.at as f32 / 10_000.0;
                    frame[0] = v;
                    frame[1] = -v;
                    self.at += 1;
                }
                self.left -= n as u64;
                Ok(n)
            }
            fn seek(&mut self, _frame: u64) -> Result<(), engine_audio::DecodeError> {
                Ok(())
            }
        }
        let mut ramp = Ramp {
            left: 10_000,
            at: 0,
        };
        let w = of_decoder(&mut ramp, 10).unwrap();
        assert_eq!(w.frames, 10_000);
        assert_eq!(w.columns.len(), 10);
        // Each column's highest sample grows along the file, its lowest mirrors it.
        for pair in w.columns.windows(2) {
            assert!(pair[1][1] > pair[0][1], "{pair:?}");
            assert!((pair[1][0] + pair[1][1]).abs() < 1e-6);
        }
        assert!((w.peak - 0.9999).abs() < 1e-3, "{}", w.peak);
        // Each channel by itself: the left rises, the right falls.
        for pair in w.channel_columns[0].windows(2) {
            assert!(pair[1][1] > pair[0][1] && pair[1][0] >= 0.0, "{pair:?}");
        }
        for pair in w.channel_columns[1].windows(2) {
            assert!(pair[1][0] < pair[0][0] && pair[1][1] <= 0.0, "{pair:?}");
        }
        // Silent while the ramp is at -60 dBFS or below (its first 11 frames), none at the end.
        assert_eq!(w.silence, [11, 0]);
    }
}
