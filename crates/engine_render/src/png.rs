//! A small PNG encoder for screenshots and golden-image tests: 8-bit RGB or RGBA, one IDAT
//! chunk, deflate with fixed Huffman codes and LZ77 matches (hash chains over a 32 KiB
//! window). Rendered UI and flat test images compress well; photographic content less so.
//! Written from RFC 1950, RFC 1951 and the PNG specification.

/// Pixel layout of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngFormat {
    /// 3 bytes per pixel.
    Rgb,
    /// 4 bytes per pixel.
    Rgba,
}

impl PngFormat {
    fn bytes(self) -> usize {
        match self {
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }

    fn color_type(self) -> u8 {
        match self {
            Self::Rgb => 2,
            Self::Rgba => 6,
        }
    }
}

/// Encodes `pixels` (rows top to bottom, tightly packed) as a PNG file. Returns `None`
/// when the buffer does not match the size.
pub fn encode_png(width: u32, height: u32, format: PngFormat, pixels: &[u8]) -> Option<Vec<u8>> {
    let stride = width as usize * format.bytes();
    if width == 0 || height == 0 || pixels.len() != stride * height as usize {
        return None;
    }
    // Scanlines, each with filter type 0 (none): LZ77 finds the repetition across rows.
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in pixels.chunks_exact(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut out = Vec::with_capacity(raw.len() / 8 + 64);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']);
    let mut header = [0u8; 13];
    header[0..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8] = 8;
    header[9] = format.color_type();
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib(&raw));
    chunk(&mut out, b"IEND", &[]);
    Some(out)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut n = 0;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[n] = c;
            n += 1;
        }
        table
    };
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c = TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for block in data.chunks(5552) {
        for &x in block {
            a += u32::from(x);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// zlib stream: header, one fixed-Huffman deflate block, Adler-32.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::default();
    // CMF: deflate, 32 KiB window; FLG: check bits for (CMF * 256 + FLG) % 31 == 0.
    w.bytes.extend_from_slice(&[0x78, 0x01]);
    deflate_fixed(data, &mut w);
    w.flush();
    w.bytes.extend_from_slice(&adler32(data).to_be_bytes());
    w.bytes
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: u64,
    count: u32,
}

impl BitWriter {
    /// Appends the low `n` bits of `value`, least significant first.
    fn put(&mut self, value: u32, n: u32) {
        self.bits |= u64::from(value) << self.count;
        self.count += n;
        while self.count >= 8 {
            self.bytes.push(self.bits as u8);
            self.bits >>= 8;
            self.count -= 8;
        }
    }

    /// Appends a Huffman code (most significant bit first).
    fn code(&mut self, code: u32, n: u32) {
        self.put(code.reverse_bits() >> (32 - n), n);
    }

    fn flush(&mut self) {
        if self.count > 0 {
            self.bytes.push(self.bits as u8);
            self.bits = 0;
            self.count = 0;
        }
    }
}

/// Fixed literal/length code of `symbol` (RFC 1951 §3.2.6).
fn literal_code(symbol: u32) -> (u32, u32) {
    match symbol {
        0..=143 => (0x30 + symbol, 8),
        144..=255 => (0x190 + symbol - 144, 9),
        256..=279 => (symbol - 256, 7),
        _ => (0xc0 + symbol - 280, 8),
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

const WINDOW: usize = 32 * 1024;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
/// Candidates tried per position: speed against ratio.
const CHAIN: usize = 32;

fn hash3(data: &[u8], i: usize) -> usize {
    let v = u32::from(data[i]) | u32::from(data[i + 1]) << 8 | u32::from(data[i + 2]) << 16;
    (v.wrapping_mul(0x9e37_79b1) >> (32 - HASH_BITS)) as usize
}

fn deflate_fixed(data: &[u8], w: &mut BitWriter) {
    w.put(1, 1); // last block
    w.put(1, 2); // fixed Huffman codes
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; WINDOW];
    let insert = |head: &mut [usize], prev: &mut [usize], i: usize| {
        if i + MIN_MATCH <= data.len() {
            let h = hash3(data, i);
            prev[i % WINDOW] = head[h];
            head[h] = i;
        }
    };
    let mut i = 0;
    while i < data.len() {
        let (mut best_len, mut best_dist) = (0, 0);
        if i + MIN_MATCH <= data.len() {
            let mut candidate = head[hash3(data, i)];
            let max = MAX_MATCH.min(data.len() - i);
            for _ in 0..CHAIN {
                if candidate == usize::MAX || i - candidate > WINDOW || candidate >= i {
                    break;
                }
                let len = data[candidate..]
                    .iter()
                    .zip(&data[i..i + max])
                    .take_while(|(a, b)| a == b)
                    .count();
                if len > best_len {
                    (best_len, best_dist) = (len, i - candidate);
                    if len == max {
                        break;
                    }
                }
                candidate = prev[candidate % WINDOW];
            }
        }
        if best_len >= MIN_MATCH {
            let l = LENGTH_BASE
                .iter()
                .rposition(|&b| usize::from(b) <= best_len)
                .unwrap_or(0);
            let (code, n) = literal_code(257 + l as u32);
            w.code(code, n);
            w.put(
                (best_len - usize::from(LENGTH_BASE[l])) as u32,
                u32::from(LENGTH_EXTRA[l]),
            );
            let d = DIST_BASE
                .iter()
                .rposition(|&b| usize::from(b) <= best_dist)
                .unwrap_or(0);
            w.code(d as u32, 5);
            w.put(
                (best_dist - usize::from(DIST_BASE[d])) as u32,
                u32::from(DIST_EXTRA[d]),
            );
            for j in i..i + best_len {
                insert(&mut head, &mut prev, j);
            }
            i += best_len;
        } else {
            let (code, n) = literal_code(u32::from(data[i]));
            w.code(code, n);
            insert(&mut head, &mut prev, i);
            i += 1;
        }
    }
    let (code, n) = literal_code(256);
    w.code(code, n);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal inflater for fixed-Huffman blocks, to check the encoder's output.
    fn inflate_fixed(stream: &[u8]) -> Vec<u8> {
        struct Bits<'a> {
            data: &'a [u8],
            pos: usize,
        }
        impl Bits<'_> {
            fn bit(&mut self) -> u32 {
                let b = (self.data[self.pos / 8] >> (self.pos % 8)) & 1;
                self.pos += 1;
                u32::from(b)
            }
            fn get(&mut self, n: u32) -> u32 {
                (0..n).fold(0, |v, i| v | self.bit() << i)
            }
            fn code(&mut self, n: u32) -> u32 {
                (0..n).fold(0, |v, _| v << 1 | self.bit())
            }
        }
        let mut r = Bits {
            data: stream,
            pos: 0,
        };
        assert_eq!(r.get(1), 1, "last block");
        assert_eq!(r.get(2), 1, "fixed codes");
        let mut out: Vec<u8> = Vec::new();
        loop {
            // Read 7 bits, extend as the fixed code lengths require.
            let mut c = r.code(7);
            let symbol = if c <= 0x17 {
                c + 256
            } else {
                c = c << 1 | r.bit();
                if (0x30..=0xbf).contains(&c) {
                    c - 0x30
                } else if (0xc0..=0xc7).contains(&c) {
                    c - 0xc0 + 280
                } else {
                    c = c << 1 | r.bit();
                    c - 0x190 + 144
                }
            };
            match symbol {
                0..=255 => out.push(symbol as u8),
                256 => break,
                _ => {
                    let l = (symbol - 257) as usize;
                    let len =
                        usize::from(LENGTH_BASE[l]) + r.get(u32::from(LENGTH_EXTRA[l])) as usize;
                    let d = r.code(5) as usize;
                    let dist = usize::from(DIST_BASE[d]) + r.get(u32::from(DIST_EXTRA[d])) as usize;
                    let start = out.len() - dist;
                    for k in 0..len {
                        out.push(out[start + k]);
                    }
                }
            }
        }
        out
    }

    fn decode(png: &[u8]) -> (u32, u32, u8, Vec<u8>) {
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
        );
        let mut pos = 8;
        let (mut width, mut height, mut color) = (0, 0, 0);
        let mut idat = Vec::new();
        while pos < png.len() {
            let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &png[pos + 4..pos + 8];
            let data = &png[pos + 8..pos + 8 + len];
            let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
            assert_eq!(crc, crc32(&png[pos + 4..pos + 8 + len]), "chunk CRC");
            match kind {
                b"IHDR" => {
                    width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                    height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                    color = data[9];
                }
                b"IDAT" => idat.extend_from_slice(data),
                _ => {}
            }
            pos += 12 + len;
        }
        assert_eq!(idat[0] & 0x0f, 8, "deflate");
        assert_eq!((u32::from(idat[0]) * 256 + u32::from(idat[1])) % 31, 0);
        let raw = inflate_fixed(&idat[2..idat.len() - 4]);
        let adler = u32::from_be_bytes(idat[idat.len() - 4..].try_into().unwrap());
        assert_eq!(adler, adler32(&raw));
        (width, height, color, raw)
    }

    #[test]
    fn round_trips_flat_and_noisy_images() {
        let (w, h) = (97u32, 61u32);
        let mut seed = 0x1234_5678u32;
        for format in [PngFormat::Rgb, PngFormat::Rgba] {
            let bpp = format.bytes();
            let mut pixels = vec![0u8; w as usize * h as usize * bpp];
            for (i, p) in pixels.iter_mut().enumerate() {
                let x = (i / bpp) % w as usize;
                *p = if x < 40 {
                    // Flat areas and stripes: long matches.
                    ((x / 10) * 40 + i % bpp) as u8
                } else {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                };
            }
            let png = encode_png(w, h, format, &pixels).unwrap();
            let (dw, dh, color, raw) = decode(&png);
            assert_eq!((dw, dh, color), (w, h, format.color_type()));
            let stride = w as usize * bpp;
            for (row, line) in raw.chunks_exact(stride + 1).enumerate() {
                assert_eq!(line[0], 0);
                assert_eq!(&line[1..], &pixels[row * stride..(row + 1) * stride]);
            }
        }
    }

    #[test]
    fn flat_images_compress_well() {
        let pixels = vec![40u8; 800 * 600 * 3];
        let png = encode_png(800, 600, PngFormat::Rgb, &pixels).unwrap();
        assert!(png.len() < 20_000, "{} bytes", png.len());
        assert!(encode_png(800, 600, PngFormat::Rgb, &pixels[1..]).is_none());
    }
}
