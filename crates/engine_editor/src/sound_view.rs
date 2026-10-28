//! The inspector's sound file (ADR 0107 E11-3c, `docs/research/ux/sound.md`, previews): its
//! waveform a band per channel at the inspector's width, read whole on a thread of its own
//! when the file is picked, and what the same read says about it: its length, rate and
//! channels, its peak, and the silence at its ends.
//!
//! The picture is drawn at the size the panel shows it, in the window's pixels (a column
//! a pixel: sampled smaller, a waveform's columns would alias into stripes), and uploaded
//! once into the corner of a texture of the editor's overlay ([`TEXTURE`]).

use engine_scene::AssetId;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::mpsc;

/// The overlay's texture the picture goes into: room for the panel at a UI scale of 3.
pub(crate) const TEXTURE: [u32; 2] = [1024, 256];

/// The sounds' colour (the dock's), a column that reaches full scale (it may clip), and
/// the faint line through each band's middle.
const WAVE: [u8; 4] = [180, 130, 220, 255];
const CLIPS: [u8; 4] = [240, 110, 90, 255];
const MIDDLE: [u8; 4] = [180, 130, 220, 90];

/// Silence this short at an end is not mentioned.
const QUIET_ENOUGH: f64 = 0.005;

/// What a read of the file gave: its picture (and its size) and its facts.
struct Read {
    pixels: Vec<u8>,
    size: [u32; 2],
    channels: u16,
    rate: u32,
    seconds: f64,
    peak_dbfs: f32,
    silent: bool,
    silence: [f64; 2],
}

/// The sound file the inspector shows.
pub(crate) struct SoundView {
    /// Its asset.
    pub(crate) id: AssetId,
    /// The read under way.
    job: Option<mpsc::Receiver<Result<Read, String>>>,
    /// The picture, read and not yet in the texture.
    upload: Option<Vec<u8>>,
    /// The picture's size, in the texture's corner.
    pub(crate) size: [u32; 2],
    /// The picture is in the texture.
    pub(crate) shown: bool,
    /// Why it could not be read.
    pub(crate) failed: Option<String>,
    /// Its channels (a band each), length in seconds, and silence at its ends as parts of
    /// its length, once read.
    pub(crate) channels: u16,
    pub(crate) seconds: Option<f64>,
    pub(crate) silence: [f32; 2],
    /// The facts' texts in the inspector, once read.
    pub(crate) length: String,
    pub(crate) format: String,
    pub(crate) peak: String,
    pub(crate) quiet: String,
    /// "0:01.20 / 0:03.20", written each frame.
    pub(crate) time: String,
    /// Plays looped (the panel's toggle).
    pub(crate) looping: bool,
    /// Where along the waveform a drag holds the playhead, 0 to 1, and where it was
    /// pressed (a release elsewhere plays from there).
    pub(crate) scrub: Option<(f32, f32)>,
}

impl SoundView {
    /// Sound file `id` at `path` shown at UI scale `scale`, its read started on a thread
    /// of its own.
    pub(crate) fn open(id: AssetId, path: PathBuf, scale: f32) -> Self {
        let (send, answer) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name(String::from("ofelia-sound-view"))
            .spawn(move || {
                let _ = send.send(read(&path, scale));
            });
        let (job, failed) = match spawned {
            Ok(_) => (Some(answer), None),
            Err(e) => (None, Some(e.to_string())),
        };
        Self {
            id,
            job,
            upload: None,
            size: [0, 0],
            shown: false,
            failed,
            channels: 1,
            seconds: None,
            silence: [0.0; 2],
            length: String::new(),
            format: String::new(),
            peak: String::new(),
            quiet: String::new(),
            time: String::with_capacity(32),
            looping: false,
            scrub: None,
        }
    }

    /// Takes the read's answer when it has come: true when it did.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(job) = &self.job else {
            return false;
        };
        let answer = match job.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(String::from("the reading thread stopped"))
            }
        };
        self.job = None;
        match answer {
            Ok(read) => self.facts(read),
            Err(why) => self.failed = Some(why),
        }
        true
    }

    /// The read's facts written, its picture kept for the texture.
    fn facts(&mut self, read: Read) {
        self.channels = read.channels.max(1);
        self.seconds = Some(read.seconds);
        let share = |s: f64| (s / read.seconds.max(1e-9)).clamp(0.0, 1.0) as f32;
        self.silence = if read.silent {
            [0.0; 2]
        } else {
            [share(read.silence[0]), share(read.silence[1])]
        };
        self.length.clear();
        clock(&mut self.length, read.seconds);
        self.format.clear();
        let kilo = f64::from(read.rate) / 1000.0;
        let _ = if read.rate.is_multiple_of(1000) {
            write!(self.format, "{kilo:.0} kHz")
        } else {
            write!(self.format, "{kilo:.1} kHz")
        };
        self.format.push_str(match read.channels {
            1 => " · mono",
            2 => " · stereo",
            _ => " · several channels",
        });
        self.peak.clear();
        self.quiet.clear();
        if read.silent {
            self.peak.push_str("silent");
            self.quiet.push_str("all of it");
        } else {
            // Negative levels with a minus sign, as dB are written.
            let db = read.peak_dbfs;
            let _ = if db < -0.05 {
                write!(self.peak, "\u{2212}{:.1} dBFS", -db)
            } else {
                write!(self.peak, "{:.1} dBFS", db.max(0.0))
            };
            if db >= -0.1 {
                self.peak.push_str(" (it may clip)");
            }
            let [head, tail] = read.silence;
            match (head > QUIET_ENOUGH, tail > QUIET_ENOUGH) {
                (false, false) => self.quiet.push_str("none"),
                (true, false) => {
                    let _ = write!(self.quiet, "{head:.2} s first");
                }
                (false, true) => {
                    let _ = write!(self.quiet, "{tail:.2} s last");
                }
                (true, true) => {
                    let _ = write!(self.quiet, "{head:.2} s first \u{b7} {tail:.2} s last");
                }
            }
        }
        self.upload = Some(read.pixels);
        self.size = read.size;
        self.shown = false;
    }

    /// The picture to put into the texture's corner, and its size, once (the overlay
    /// calls it while it draws).
    pub(crate) fn take_upload(&mut self) -> Option<([u32; 2], Vec<u8>)> {
        let pixels = self.upload.take()?;
        self.shown = true;
        Some((self.size, pixels))
    }

    /// The picture's part of the texture: min u, min v, max u, max v.
    pub(crate) fn uv(&self) -> [f32; 4] {
        [
            0.0,
            0.0,
            self.size[0] as f32 / TEXTURE[0] as f32,
            self.size[1] as f32 / TEXTURE[1] as f32,
        ]
    }

    /// Still being read.
    pub(crate) fn reading(&self) -> bool {
        self.job.is_some()
    }

    /// `time` written for a playhead at `at` seconds of its length (the length unknown:
    /// only where it is).
    pub(crate) fn write_time(&mut self, at: f64) {
        self.time.clear();
        clock(&mut self.time, at);
        if let Some(seconds) = self.seconds {
            self.time.push_str(" / ");
            clock(&mut self.time, seconds);
        }
    }
}

/// File `path` read whole: its waveform drawn at the panel's size at UI scale `scale`, its
/// facts.
fn read(path: &std::path::Path, scale: f32) -> Result<Read, String> {
    engine_profiler::zone!("editor::sound_view::read");
    let pixels = |logical: f32, room: u32| ((logical * scale).round() as u32).clamp(16, room);
    let width = pixels(crate::shell::SOUND_WIDTH, TEXTURE[0]);
    let wave = engine_app::waveform(path, width as usize)?;
    let bands = f32::from(wave.channels.clamp(1, 2));
    let size = [width, pixels(crate::shell::SOUND_BAND * bands, TEXTURE[1])];
    Ok(Read {
        pixels: picture(&wave, size),
        size,
        channels: wave.channels,
        rate: wave.sample_rate,
        seconds: wave.seconds(),
        peak_dbfs: wave.peak_dbfs(),
        silent: wave.is_silent(),
        silence: [
            wave.seconds_of(wave.silence[0]),
            wave.seconds_of(wave.silence[1]),
        ],
    })
}

/// A waveform drawn into `size` on transparency, a band per channel (the first on top).
pub(crate) fn picture(wave: &engine_app::Waveform, size: [u32; 2]) -> Vec<u8> {
    let (width, height) = (size[0] as usize, size[1] as usize);
    let mut out = vec![0u8; width * height * 4];
    let bands = wave.channel_columns.len().max(1);
    let tall = height / bands;
    for (c, columns) in wave.channel_columns.iter().enumerate() {
        band(&mut out, width, c * tall, tall, 4, columns);
    }
    out
}

/// One band of a waveform drawn into `out` (`width` pixels a row): `height` rows from row
/// `top`, `margin` of them kept clear above and below; a faint middle line, each column
/// from its lowest to its highest sample, one at full scale in a warning colour.
pub(crate) fn band(
    out: &mut [u8],
    width: usize,
    top: usize,
    height: usize,
    margin: usize,
    columns: &[[f32; 2]],
) {
    let middle = top + height / 2;
    let half = (height / 2).saturating_sub(margin) as f32;
    let bottom = top + height.saturating_sub(1);
    let mut put = |x: usize, y: usize, color: [u8; 4]| {
        let o = (y.clamp(top, bottom) * width + x) * 4;
        if let Some(pixel) = out.get_mut(o..o + 4) {
            pixel.copy_from_slice(&color);
        }
    };
    for x in 0..width {
        put(x, middle, MIDDLE);
    }
    for (x, &[low, high]) in columns.iter().take(width).enumerate() {
        let color = if high >= 0.999 || low <= -0.999 {
            CLIPS
        } else {
            WAVE
        };
        let row = |v: f32| (middle as f32 - v.clamp(-1.0, 1.0) * half).round() as usize;
        let (a, b) = (row(high), row(low));
        for y in a.min(b)..=a.max(b) {
            put(x, y, color);
        }
    }
}

/// `seconds` written as minutes, seconds and hundredths: "1:02.35".
pub(crate) fn clock(out: &mut String, seconds: f64) {
    let hundredths = (seconds.max(0.0) * 100.0).round() as u64;
    let (minutes, rest) = (hundredths / 6000, hundredths % 6000);
    let _ = write!(out, "{minutes}:{:02}.{:02}", rest / 100, rest % 100);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn wave(channels: Vec<Vec<[f32; 2]>>) -> engine_app::Waveform {
        engine_app::Waveform {
            columns: channels[0].clone(),
            channel_columns: channels,
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            peak: 1.0,
            silence: [0, 0],
        }
    }

    const WIDTH: usize = 300;
    const HEIGHT: usize = 88;

    /// The rows of column `x` of `pixels` that are drawn opaque.
    fn drawn(pixels: &[u8], x: usize) -> Vec<usize> {
        (0..HEIGHT)
            .filter(|&y| pixels[(y * WIDTH + x) * 4 + 3] == 255)
            .collect()
    }

    #[test]
    fn each_channel_is_drawn_in_its_own_band() {
        let quiet = vec![[0.0, 0.0]; WIDTH];
        let mut loud = vec![[-0.5, 0.5]; WIDTH];
        loud[10] = [-1.0, 1.0];
        let pixels = picture(&wave(vec![loud, quiet]), [WIDTH as u32, HEIGHT as u32]);
        assert_eq!(pixels.len(), WIDTH * HEIGHT * 4);
        let band = HEIGHT / 2;
        // The left on top, swinging; the right below, silence a line through its middle.
        let (left, right): (Vec<usize>, Vec<usize>) =
            drawn(&pixels, 5).into_iter().partition(|&y| y < band);
        assert!(left.len() > band / 3, "{left:?}");
        assert_eq!(right, [band + band / 2]);
        // Full scale in the warning colour, from its band's top margin to its bottom one.
        let o = ((band / 2) * WIDTH + 10) * 4;
        assert_eq!(pixels[o..o + 4], CLIPS);
        let (full, below): (Vec<usize>, Vec<usize>) =
            drawn(&pixels, 10).into_iter().partition(|&y| y < band);
        assert_eq!((full[0], full[full.len() - 1]), (4, band - 4), "{full:?}");
        assert_eq!(below, [band + band / 2], "the right band untouched");
    }

    #[test]
    fn times_read_as_minutes_seconds_and_hundredths() {
        let mut s = String::new();
        clock(&mut s, 0.0);
        s.push(' ');
        clock(&mut s, 3.2);
        s.push(' ');
        clock(&mut s, 161.074);
        s.push(' ');
        clock(&mut s, 59.996);
        assert_eq!(s, "0:00.00 0:03.20 2:41.07 1:00.00");
    }

    #[test]
    fn the_facts_say_the_peak_and_the_silence_at_the_ends() {
        let mut view = SoundView::open(AssetId::new(), PathBuf::from("none.wav"), 1.0);
        view.facts(Read {
            pixels: Vec::new(),
            size: [300, 88],
            channels: 2,
            rate: 44_100,
            seconds: 3.2,
            peak_dbfs: -0.04,
            silent: false,
            silence: [0.18, 0.0],
        });
        assert_eq!(view.length, "0:03.20");
        assert_eq!(view.format, "44.1 kHz \u{b7} stereo");
        assert_eq!(view.peak, "0.0 dBFS (it may clip)");
        assert_eq!(view.quiet, "0.18 s first");
        assert!((view.silence[0] - 0.18 / 3.2).abs() < 1e-6);
        view.facts(Read {
            pixels: Vec::new(),
            size: [300, 44],
            channels: 1,
            rate: 48_000,
            seconds: 2.0,
            peak_dbfs: -6.02,
            silent: false,
            silence: [0.0, 0.4],
        });
        assert_eq!(view.format, "48 kHz \u{b7} mono");
        assert_eq!(view.peak, "\u{2212}6.0 dBFS");
        assert_eq!(view.quiet, "0.40 s last");
        view.write_time(1.2);
        assert_eq!(view.time, "0:01.20 / 0:02.00");
        // A file that cannot be read says why.
        let started = std::time::Instant::now();
        while !view.poll() && started.elapsed().as_secs() < 5 {
            std::thread::yield_now();
        }
        assert!(view.failed.is_some(), "a missing file");
    }
}
