//! A video's Opus track decoded from the file's packets (ADR 0100): the test clip's
//! 440 Hz tone, its length, and a seek that lands where decoding straight through does.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use engine_audio::Decoder;
use engine_video::audio::VideoAudio;
use engine_video::index::FileIndex;

fn tone() -> VideoAudio {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/clips/svt_8bit_320x180.webm"
    );
    let data: Arc<[u8]> = std::fs::read(path).unwrap().into();
    let index = FileIndex::read(&data).unwrap();
    let track = index.audio.clone().unwrap();
    VideoAudio::new(data, index.packets.into(), &track.private).unwrap()
}

fn read_all(audio: &mut VideoAudio) -> Vec<f32> {
    let mut out = Vec::new();
    let mut chunk = vec![0.0f32; 4096];
    loop {
        let frames = audio.read(&mut chunk).unwrap();
        if frames == 0 {
            return out;
        }
        out.extend_from_slice(&chunk[..frames * 2]);
    }
}

#[test]
fn the_tone_plays_for_its_two_seconds() {
    let mut audio = tone();
    assert_eq!((audio.sample_rate(), audio.channels()), (48_000, 2));
    let samples = read_all(&mut audio);
    let frames = samples.len() / 2;
    // Two seconds, give or take the last packet's padding (WebM's DiscardPadding).
    assert!((96_000..=97_000).contains(&frames), "{frames} frames");
    assert_eq!(audio.frames(), Some(frames as u64));
    // 440 Hz: 880 zero crossings a second (the middle second, past the encoder's start).
    let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
    let second = &left[24_000..72_000];
    let crossings = second
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    assert!((875..=885).contains(&crossings), "{crossings} crossings");
}

#[test]
fn a_seek_lands_where_decoding_through_does() {
    let mut through = tone();
    let all = read_all(&mut through);
    let mut sought = tone();
    sought.seek(48_000).unwrap();
    let mut chunk = vec![0.0f32; 2 * 4_800];
    let frames = sought.read(&mut chunk).unwrap();
    assert_eq!(frames, 4_800);
    // The decoder converges over the pre-roll: the samples agree closely, not exactly.
    let largest = chunk
        .iter()
        .zip(&all[2 * 48_000..])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(largest < 0.02, "differs by {largest}");
}
