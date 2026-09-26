//! RIFF/WAVE decoding to interleaved `f32`.
//!
//! Reads the `fmt ` and `data` chunks and skips everything else (`LIST`, `fact`, `cue `,
//! ...). Bounds are checked on every read, so malformed input is an error, never a panic.

use crate::clip::ClipError;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

struct Format {
    tag: u16,
    channels: u16,
    sample_rate: u32,
    bits: u16,
}

/// Returns `(sample_rate, channels, interleaved samples)`.
pub(crate) fn decode(bytes: &[u8]) -> Result<(u32, u16, Vec<f32>), ClipError> {
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err(ClipError::NotWav);
    }
    let mut format: Option<Format> = None;
    let mut data: Option<&[u8]> = None;
    let mut at = 12usize;
    while let (Some(id), Some(size)) = (bytes.get(at..at + 4), u32_at(bytes, at + 4)) {
        let body_start = at + 8;
        let size = size as usize;
        // A truncated final chunk (common for `data` in streamed recordings) is read up to
        // the end of the file.
        let body =
            &bytes[body_start.min(bytes.len())..body_start.saturating_add(size).min(bytes.len())];
        match id {
            b"fmt " => {
                let mut tag = u16_at(body, 0).ok_or(ClipError::Truncated)?;
                let channels = u16_at(body, 2).ok_or(ClipError::Truncated)?;
                let sample_rate = u32_at(body, 4).ok_or(ClipError::Truncated)?;
                let bits = u16_at(body, 14).ok_or(ClipError::Truncated)?;
                if tag == WAVE_FORMAT_EXTENSIBLE {
                    // WAVEFORMATEXTENSIBLE: the sub-format GUID starts with the real tag.
                    tag = u16_at(body, 24).ok_or(ClipError::Truncated)?;
                }
                format = Some(Format {
                    tag,
                    channels,
                    sample_rate,
                    bits,
                });
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are padded to an even size.
        at = body_start.saturating_add(size).saturating_add(size & 1);
    }
    let (Some(format), Some(data)) = (format, data) else {
        return Err(ClipError::Truncated);
    };
    if format.sample_rate == 0 || !(1..=2).contains(&format.channels) {
        return Err(ClipError::Format {
            sample_rate: format.sample_rate,
            channels: format.channels,
        });
    }
    let unsupported = ClipError::UnsupportedEncoding {
        tag: format.tag,
        bits: format.bits,
    };
    let samples: Vec<f32> = match (format.tag, format.bits) {
        (WAVE_FORMAT_PCM, 8) => data
            .iter()
            .map(|&b| (f32::from(b) - 128.0) / 128.0)
            .collect(),
        (WAVE_FORMAT_PCM, 16) => data
            .chunks_exact(2)
            .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32_768.0)
            .collect(),
        (WAVE_FORMAT_PCM, 24) => data
            .chunks_exact(3)
            .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0)
            .collect(),
        (WAVE_FORMAT_PCM, 32) => data
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
            .collect(),
        (WAVE_FORMAT_IEEE_FLOAT, 32) => data
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        (WAVE_FORMAT_IEEE_FLOAT, 64) => data
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) as f32)
            .collect(),
        _ => return Err(unsupported),
    };
    // Drop a partial frame at the end rather than rejecting the file.
    let whole = samples.len() - samples.len() % usize::from(format.channels);
    let mut samples = samples;
    samples.truncate(whole);
    if samples.is_empty() {
        return Err(ClipError::Length);
    }
    Ok((format.sample_rate, format.channels, samples))
}

#[cfg(test)]
pub(crate) fn encode_pcm16(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    b.extend_from_slice(&(channels * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}
