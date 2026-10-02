//! Bit packing: a writer into a caller's buffer and a bounds-checked reader. Values are
//! written least significant bit first into 64-bit little-endian words (Fiedler's bit
//! packer). Reading past the end yields `None`: malformed input from the network is
//! dropped, never a panic.
//!
//! Integers that are usually small (deltas, gaps) use exponential-Golomb codes of order
//! `k` on their zigzag form: values below `2^k` take `k + 1` bits, each doubling of the
//! range two more.

/// Writes bits into a byte buffer that grows only up to its capacity limit.
#[derive(Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    scratch: u64,
    scratch_bits: u32,
    bits: usize,
    limit_bits: usize,
}

impl BitWriter {
    /// A writer that refuses to grow past `limit_bytes` (its buffer is reserved once).
    pub fn with_limit(limit_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit_bytes + 8),
            limit_bits: limit_bytes * 8,
            ..Self::default()
        }
    }

    /// Starts over with a new limit; keeps the buffer (no allocation once it is large
    /// enough).
    pub fn reset(&mut self, limit_bytes: usize) {
        self.bytes.clear();
        if self.bytes.capacity() < limit_bytes + 8 {
            self.bytes.reserve(limit_bytes + 8);
        }
        self.scratch = 0;
        self.scratch_bits = 0;
        self.bits = 0;
        self.limit_bits = limit_bytes * 8;
    }

    /// Bits written.
    pub fn bits(&self) -> usize {
        self.bits
    }

    /// Bits still allowed.
    pub fn remaining_bits(&self) -> usize {
        self.limit_bits.saturating_sub(self.bits)
    }

    /// Whether the writer went over its limit (then its contents are invalid).
    pub fn overflowed(&self) -> bool {
        self.bits > self.limit_bits
    }

    /// Goes back to `mark` (an earlier [`Self::bits`]), dropping what was written after
    /// it: for undoing a record that did not fit.
    pub fn rewind(&mut self, mark: usize) {
        debug_assert!(mark <= self.bits);
        let word = mark / 32;
        let rem = (mark % 32) as u32;
        let keep = (1u64 << rem) - 1;
        if self.bytes.len() >= (word + 1) * 4 {
            let mut w = [0u8; 4];
            w.copy_from_slice(&self.bytes[word * 4..word * 4 + 4]);
            self.scratch = u64::from(u32::from_le_bytes(w)) & keep;
        } else {
            self.scratch &= keep;
        }
        self.bytes.truncate(word * 4);
        self.scratch_bits = rem;
        self.bits = mark;
    }

    /// Writes the low `n` bits of `value` (`n <= 32`).
    #[inline]
    pub fn write(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        if n == 0 {
            return;
        }
        let v = u64::from(value) & ((1u64 << n) - 1);
        self.scratch |= v << self.scratch_bits;
        self.scratch_bits += n;
        self.bits += n as usize;
        if self.scratch_bits >= 32 {
            // Past the limit the contents are invalid anyway (the caller rewinds); never
            // grow the buffer, so a full packet does not allocate.
            if self.bytes.len() + 4 <= self.bytes.capacity() {
                self.bytes
                    .extend_from_slice(&(self.scratch as u32).to_le_bytes());
            }
            self.scratch >>= 32;
            self.scratch_bits -= 32;
        }
    }

    /// Writes one bit.
    #[inline]
    pub fn bit(&mut self, b: bool) {
        self.write(u32::from(b), 1);
    }

    /// Writes an unsigned value with an order-`k` exponential-Golomb code.
    #[inline]
    pub fn golomb(&mut self, value: u32, k: u32) {
        let m = u64::from(value) + (1u64 << k);
        let len = 64 - m.leading_zeros(); // bits of m, at least k + 1
        let zeros = len - k - 1;
        // `zeros` zero bits, then m's `len` bits from the most significant down. LSB-first
        // packing: write the prefix, then m reversed would cost a loop; instead the reader
        // reads the prefix length and then m's bits as one little-endian field.
        self.write(0, zeros.min(32));
        if zeros > 32 {
            self.write(0, zeros - 32);
        }
        self.write(1, 1);
        // m without its leading one bit: len - 1 bits.
        let rest = m & ((1u64 << (len - 1)) - 1);
        let n = len - 1;
        if n > 32 {
            self.write(rest as u32, 32);
            self.write((rest >> 32) as u32, n - 32);
        } else {
            self.write(rest as u32, n);
        }
    }

    /// Writes a signed value with an order-`k` exponential-Golomb code of its zigzag
    /// form.
    #[inline]
    pub fn signed(&mut self, value: i32, k: u32) {
        self.golomb(zigzag(value), k);
    }

    /// Finishes and returns the bytes (padded to a whole byte).
    pub fn finish(&mut self) -> &[u8] {
        let bytes = self.bits.div_ceil(8);
        let pending = self.scratch.to_le_bytes();
        let have = self.bytes.len();
        if bytes > have {
            self.bytes.extend_from_slice(&pending[..bytes - have]);
        }
        self.bytes.truncate(bytes);
        self.scratch = 0;
        self.scratch_bits = 0;
        &self.bytes
    }
}

/// Reads bits written by [`BitWriter`].
#[derive(Debug)]
pub struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// Reads `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Bits read.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Reads `n <= 32` bits.
    #[inline]
    pub fn read(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Some(0);
        }
        let end = self.pos + n as usize;
        if end > self.bytes.len() * 8 {
            return None;
        }
        let byte = self.pos / 8;
        let shift = (self.pos % 8) as u32;
        // Up to 40 bits span 5..8 bytes; read 8 when available.
        let mut word = [0u8; 8];
        let take = (self.bytes.len() - byte).min(8);
        word[..take].copy_from_slice(&self.bytes[byte..byte + take]);
        let v = (u64::from_le_bytes(word) >> shift) & ((1u64 << n) - 1);
        self.pos = end;
        Some(v as u32)
    }

    /// Reads one bit.
    #[inline]
    pub fn bit(&mut self) -> Option<bool> {
        self.read(1).map(|b| b != 0)
    }

    /// Reads an order-`k` exponential-Golomb code.
    #[inline]
    pub fn golomb(&mut self, k: u32) -> Option<u32> {
        let mut zeros = 0u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 40 {
                return None;
            }
        }
        let n = zeros + k; // bits after the leading one
        let rest = if n > 32 {
            let lo = u64::from(self.read(32)?);
            lo | (u64::from(self.read(n - 32)?) << 32)
        } else {
            u64::from(self.read(n)?)
        };
        let m = (1u64 << n) | rest;
        u32::try_from(m - (1u64 << k)).ok()
    }

    /// Reads a signed order-`k` exponential-Golomb code.
    #[inline]
    pub fn signed(&mut self, k: u32) -> Option<i32> {
        self.golomb(k).map(unzigzag)
    }
}

/// Maps signed to unsigned so small magnitudes stay small: 0, -1, 1, -2, ... → 0, 1, 2, 3.
#[inline]
pub const fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

/// Inverse of [`zigzag`].
#[inline]
pub const fn unzigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

/// Bits an order-`k` exponential-Golomb code of `value` takes.
#[inline]
pub const fn golomb_bits(value: u32, k: u32) -> u32 {
    let m = value as u64 + (1u64 << k);
    let len = 64 - m.leading_zeros();
    2 * len - k - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_fields_and_codes() {
        let mut w = BitWriter::with_limit(4096);
        let mut x = 0x1234_5678u32;
        let mut values = Vec::new();
        for i in 0..2000u32 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let n = 1 + (x % 32);
            let v = x >> (32 - n);
            let s = (x as i32) >> (i % 31);
            let k = i % 5;
            w.write(v, n);
            w.signed(s, k);
            w.golomb(v, k);
            values.push((v, n, s, k));
        }
        assert!(!w.overflowed() || w.bits() > 4096 * 8);
        let mut w2 = BitWriter::with_limit(1 << 20);
        for &(v, n, s, k) in &values {
            w2.write(v, n);
            w2.signed(s, k);
            w2.golomb(v, k);
        }
        let expected_bits = w2.bits();
        let bytes = w2.finish().to_vec();
        assert_eq!(bytes.len(), expected_bits.div_ceil(8));
        let mut r = BitReader::new(&bytes);
        for &(v, n, s, k) in &values {
            assert_eq!(r.read(n), Some(v));
            assert_eq!(r.signed(k), Some(s));
            assert_eq!(r.golomb(k), Some(v));
        }
        assert_eq!(r.position(), expected_bits);
    }

    #[test]
    fn golomb_lengths_match_the_formula_and_extremes_survive() {
        for k in 0..6 {
            for v in [0u32, 1, 2, 3, 7, 8, 100, 65_535, u32::MAX - 1, u32::MAX] {
                let mut w = BitWriter::with_limit(64);
                w.golomb(v, k);
                assert_eq!(w.bits() as u32, golomb_bits(v, k), "{v} k{k}");
                let bytes = w.finish().to_vec();
                assert_eq!(BitReader::new(&bytes).golomb(k), Some(v));
            }
        }
        for v in [0, 1, -1, i32::MAX, i32::MIN] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn rewinding_drops_what_did_not_fit() {
        let mut w = BitWriter::with_limit(8);
        w.write(0b101, 3);
        let mark = w.bits();
        for i in 0..100 {
            w.golomb(i * 1000, 0);
        }
        assert!(w.overflowed());
        w.rewind(mark);
        assert!(!w.overflowed());
        w.write(0x3ff, 10);
        let bytes = w.finish().to_vec();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(3), Some(0b101));
        assert_eq!(r.read(10), Some(0x3ff));
        assert_eq!(bytes.len(), 2);
        // Rewinding inside a flushed word.
        let mut w = BitWriter::with_limit(64);
        w.write(u32::MAX, 32);
        w.write(7, 20);
        w.rewind(40);
        w.write(0, 30);
        let bytes = w.finish().to_vec();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(32), Some(u32::MAX));
        assert_eq!(r.read(8), Some(7));
        assert_eq!(r.read(30), Some(0));
    }

    #[test]
    fn truncated_input_is_refused() {
        let mut w = BitWriter::with_limit(16);
        w.golomb(1000, 0);
        let bytes = w.finish().to_vec();
        let mut r = BitReader::new(&bytes[..1]);
        assert_eq!(r.golomb(0), None);
        assert_eq!(BitReader::new(&[]).read(1), None);
        // An all-zero stream is not a code.
        assert_eq!(BitReader::new(&[0u8; 16]).golomb(0), None);
    }
}
