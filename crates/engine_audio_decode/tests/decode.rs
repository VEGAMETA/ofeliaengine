//! The decoders against files made by ffmpeg (`tests/data`, 0.5 s: left a 440 Hz sine,
//! right 1000 Hz, amplitude 0.5; the mono file has only the 440 Hz one).

#![allow(clippy::unwrap_used, clippy::float_cmp)]

use engine_audio::{Clip, DecodeError, Decoder};
use engine_audio_decode::{Format, decode, open_bytes, sniff};

fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// Everything the decoder has, reading `chunk` frames at a time.
fn read_all(decoder: &mut dyn Decoder, chunk: usize) -> Vec<f32> {
    let channels = usize::from(decoder.channels());
    let mut buffer = vec![0.0; chunk * channels];
    let mut all = Vec::new();
    loop {
        let frames = decoder.read(&mut buffer).unwrap();
        if frames == 0 {
            return all;
        }
        all.extend_from_slice(&buffer[..frames * channels]);
    }
}

/// Upward zero crossings per second.
fn frequency(samples: &[f32], rate: u32) -> f32 {
    let rising = samples
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    rising as f32 * rate as f32 / samples.len() as f32
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

struct Case {
    file: &'static str,
    format: Format,
    rate: u32,
    channels: u16,
    frames: usize,
    /// Lossless: the same signal as PCM WAV, which it must decode to exactly.
    reference: Option<&'static str>,
}

const CASES: [Case; 5] = [
    Case {
        file: "stereo_44100.ogg",
        format: Format::Vorbis,
        rate: 44_100,
        channels: 2,
        frames: 22_050,
        reference: None,
    },
    Case {
        file: "mono_22050.ogg",
        format: Format::Vorbis,
        rate: 22_050,
        channels: 1,
        frames: 11_025,
        reference: None,
    },
    Case {
        file: "stereo_48000.opus",
        format: Format::Opus,
        rate: 48_000,
        channels: 2,
        frames: 24_000,
        reference: None,
    },
    Case {
        file: "stereo_44100_s16.flac",
        format: Format::Flac,
        rate: 44_100,
        channels: 2,
        frames: 22_050,
        reference: Some("stereo_44100_s16.wav"),
    },
    Case {
        file: "stereo_48000_s24.flac",
        format: Format::Flac,
        rate: 48_000,
        channels: 2,
        frames: 24_000,
        reference: Some("stereo_48000_s24.wav"),
    },
];

#[test]
fn every_format_decodes_to_the_signal_it_was_made_from() {
    for case in &CASES {
        let bytes = data(case.file);
        assert_eq!(sniff(&bytes), Some(case.format), "{}", case.file);
        let clip = decode(bytes).unwrap();
        if let Some(reference) = case.reference {
            let wav = Clip::from_wav(&data(reference)).unwrap();
            assert_eq!(wav.frames(), clip.frames(), "{}", case.file);
            for c in 0..usize::from(case.channels) {
                assert!(clip.channel(c) == wav.channel(c), "{} ch {c}", case.file);
            }
        }
        assert_eq!(clip.sample_rate(), case.rate, "{}", case.file);
        assert_eq!(clip.channels(), case.channels, "{}", case.file);
        assert_eq!(clip.frames(), case.frames, "{}", case.file);
        let tones = [440.0f32, 1000.0];
        for (c, tone) in tones
            .into_iter()
            .enumerate()
            .take(usize::from(case.channels))
        {
            let samples = clip.channel(c);
            // Away from the edges, where lossy codecs ring.
            let middle = &samples[samples.len() / 10..samples.len() * 9 / 10];
            let f = frequency(middle, case.rate);
            assert!(
                (f - tone).abs() < tone * 0.01,
                "{} ch {c}: {f} Hz",
                case.file
            );
            let level = rms(middle);
            assert!(
                (level - 0.5 / 2f32.sqrt()).abs() < 0.02,
                "{} ch {c}: rms {level}",
                case.file
            );
        }
    }
}

#[test]
fn reading_in_pieces_and_seeking_give_the_same_samples() {
    for case in &CASES {
        let channels = usize::from(case.channels);
        let mut decoder = open_bytes(data(case.file)).unwrap();
        let whole = read_all(decoder.as_mut(), 4096);
        assert_eq!(whole.len(), case.frames * channels, "{}", case.file);

        // Odd-sized reads.
        let mut decoder = open_bytes(data(case.file)).unwrap();
        assert_eq!(read_all(decoder.as_mut(), 37), whole, "{}", case.file);

        // Back to the start after reading some, and into the middle.
        for target in [0usize, 1, 1_000, case.frames - 5] {
            let mut decoder = open_bytes(data(case.file)).unwrap();
            let mut some = vec![0.0; 777 * channels];
            decoder.read(&mut some).unwrap();
            decoder.seek(target as u64).unwrap();
            let rest = read_all(decoder.as_mut(), 500);
            assert_eq!(
                rest,
                whole[target * channels..],
                "{} seek {target}",
                case.file
            );
        }

        // Past the end: nothing more.
        let mut decoder = open_bytes(data(case.file)).unwrap();
        decoder.seek(case.frames as u64 + 10).unwrap();
        assert!(read_all(decoder.as_mut(), 64).is_empty(), "{}", case.file);
    }
}

#[test]
fn unknown_and_damaged_files_are_errors_not_panics() {
    assert_eq!(
        open_bytes(b"RIFF\0\0\0\0WAVEfmt ".to_vec()).err(),
        Some(DecodeError::NotRecognized)
    );
    assert_eq!(
        open_bytes(Vec::new()).err(),
        Some(DecodeError::NotRecognized)
    );
    for case in &CASES {
        let bytes = data(case.file);
        for len in (0..bytes.len()).step_by(97) {
            let Ok(mut decoder) = open_bytes(bytes[..len].to_vec()) else {
                continue;
            };
            let mut buffer = vec![0.0; 1024 * usize::from(decoder.channels())];
            // Either an error or fewer frames, never a panic.
            while let Ok(n) = decoder.read(&mut buffer) {
                if n == 0 {
                    break;
                }
            }
        }
        // Damage the middle.
        let mut damaged = bytes.clone();
        for b in &mut damaged[bytes.len() / 3..bytes.len() / 3 + 64] {
            *b ^= 0x5A;
        }
        if let Ok(mut decoder) = open_bytes(damaged) {
            let mut buffer = vec![0.0; 1024 * usize::from(decoder.channels())];
            while let Ok(n) = decoder.read(&mut buffer) {
                if n == 0 {
                    break;
                }
            }
        }
    }
}

#[test]
fn a_streamed_file_plays_exactly_like_its_decoded_clip() {
    use engine_audio::{Audio, AudioConfig, PlayParams};
    for case in &CASES {
        // A buffer longer than the file: all of it is decoded before playing, so the
        // comparison does not depend on the stream thread's timing.
        let (mut audio, mut renderer) = Audio::new(&AudioConfig {
            stream_buffer_ms: 2_000,
            ..AudioConfig::default()
        });
        let render = |renderer: &mut engine_audio::Renderer| {
            let mut out = Vec::new();
            for _ in 0..120 {
                let mut block = vec![0.0f32; 512];
                renderer.render(&mut block, 2);
                out.extend(block);
            }
            out
        };
        let clip = decode(data(case.file)).unwrap();
        audio.play_with(&clip, &PlayParams::default()).unwrap();
        let from_clip = render(&mut renderer);
        audio.update();
        let decoder = open_bytes(data(case.file)).unwrap();
        audio.play_stream(decoder, &PlayParams::default()).unwrap();
        let streamed = render(&mut renderer);
        assert_eq!(audio.stream_underruns(), 0, "{}", case.file);
        let worst = from_clip
            .iter()
            .zip(&streamed)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-6, "{}: {worst}", case.file);
        assert!(streamed.iter().any(|&s| s != 0.0), "{}", case.file);
    }
}

#[test]
fn an_ogg_files_length_is_read_from_its_last_page() {
    let path = |name: &str| {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name)
    };
    for name in ["mono_22050.ogg", "stereo_44100.ogg"] {
        let mut decoder = engine_audio_decode::open_file(path(name)).unwrap();
        let channels = usize::from(decoder.channels());
        let frames = (read_all(decoder.as_mut(), 4096).len() / channels) as u64;
        assert_eq!(
            engine_audio_decode::ogg_last_granule(path(name)),
            Some(frames),
            "{name}: the decoded length"
        );
    }
    // Opus: at 48 kHz with its pre-skip (a few milliseconds over the decoded length).
    let mut decoder = engine_audio_decode::open_file(path("stereo_48000.opus")).unwrap();
    let frames = (read_all(decoder.as_mut(), 4096).len() / 2) as u64;
    let granule = engine_audio_decode::ogg_last_granule(path("stereo_48000.opus")).unwrap();
    assert!(
        granule >= frames && granule - frames < 1000,
        "{granule} {frames}"
    );
    assert_eq!(
        engine_audio_decode::ogg_last_granule(path("stereo_44100_s16.wav")),
        None
    );
}
