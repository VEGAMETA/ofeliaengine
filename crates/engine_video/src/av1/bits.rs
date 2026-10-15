//! Reading an AV1 bitstream's syntax elements (the specification's section 4.10).

use super::Av1Error;

/// Reads bits most significant first, as the AV1 specification's `f(n)`.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// The next bit's position, from the first byte's most significant bit.
    position: usize,
}

impl<'a> BitReader<'a> {
    /// Reads `data` from its first bit.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// Bits read so far (`get_position()`).
    pub fn position(&self) -> usize {
        self.position
    }

    /// Bytes from the next byte boundary on.
    pub fn byte_position(&self) -> usize {
        self.position.div_ceil(8)
    }

    /// `f(1)` as a flag.
    pub fn flag(&mut self) -> Result<bool, Av1Error> {
        Ok(self.f(1)? != 0)
    }

    /// `f(n)`: an unsigned `n`-bit number, `n` up to 32.
    pub fn f(&mut self, n: u32) -> Result<u32, Av1Error> {
        debug_assert!(n <= 32);
        let mut x = 0u64;
        for _ in 0..n {
            let byte = *self
                .data
                .get(self.position >> 3)
                .ok_or(Av1Error::Truncated)?;
            let bit = (byte >> (7 - (self.position & 7))) & 1;
            x = x << 1 | u64::from(bit);
            self.position += 1;
        }
        Ok(x as u32)
    }

    /// `su(n)`: an `n`-bit two's complement number.
    pub fn su(&mut self, n: u32) -> Result<i32, Av1Error> {
        let value = self.f(n)? as i64;
        let sign = 1i64 << (n - 1);
        Ok(if value & sign != 0 {
            (value - 2 * sign) as i32
        } else {
            value as i32
        })
    }

    /// `ns(n)`: a number below `n`, coded in fewer bits when small.
    pub fn ns(&mut self, n: u32) -> Result<u32, Av1Error> {
        if n <= 1 {
            return Ok(0);
        }
        let w = floor_log2(n) + 1;
        let m = (1u32 << w) - n;
        let v = self.f(w - 1)?;
        if v < m {
            return Ok(v);
        }
        let extra = self.f(1)?;
        Ok((v << 1) - m + extra)
    }

    /// `uvlc()`: an exponential-Golomb code.
    pub fn uvlc(&mut self) -> Result<u32, Av1Error> {
        let mut leading = 0u32;
        while !self.flag()? {
            leading += 1;
            if leading >= 32 {
                return Ok(u32::MAX);
            }
        }
        let value = self.f(leading)?;
        Ok(value + ((1u64 << leading) - 1) as u32)
    }

    /// `le(n)`: an `n`-byte little-endian number (byte aligned).
    pub fn le(&mut self, n: u32) -> Result<u32, Av1Error> {
        let mut t = 0u32;
        for i in 0..n {
            t |= self.f(8)? << (8 * i);
        }
        Ok(t)
    }

    /// `leb128()`: up to 8 bytes, seven bits each, least significant first.
    pub fn leb128(&mut self) -> Result<u64, Av1Error> {
        let mut value = 0u64;
        for i in 0..8 {
            let byte = self.f(8)?;
            value |= u64::from(byte & 0x7F) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Av1Error::Corrupt("leb128 longer than 8 bytes"))
    }

    /// `byte_alignment()`: skips to the next byte boundary.
    pub fn byte_alignment(&mut self) {
        self.position = self.position.div_ceil(8) * 8;
    }
}

/// `FloorLog2(x)` for `x > 0`.
pub fn floor_log2(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// `leb128()` at the start of `data`: its value and length in bytes.
pub fn leb128(data: &[u8]) -> Result<(u64, usize), Av1Error> {
    let mut value = 0u64;
    for (i, &byte) in data.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err(Av1Error::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_and_signed_fields() {
        let mut r = BitReader::new(&[0b1011_0011, 0b1100_0000]);
        assert_eq!(r.f(1), Ok(1));
        assert_eq!(r.f(3), Ok(0b011));
        assert_eq!(r.su(4), Ok(0b0011));
        assert_eq!(r.su(2), Ok(-1));
        assert_eq!(r.position(), 10);
    }

    #[test]
    fn non_symmetric_numbers() {
        // ns(5): w = 3, m = 3; v = 0b10 (2 < 3) reads 2 bits.
        let mut r = BitReader::new(&[0b1000_0000]);
        assert_eq!(r.ns(5), Ok(2));
        assert_eq!(r.position(), 2);
        // v = 0b11 (3 >= 3): one more bit; (3 << 1) - 3 + 1 = 4.
        let mut r = BitReader::new(&[0b1110_0000]);
        assert_eq!(r.ns(5), Ok(4));
        assert_eq!(r.position(), 3);
    }

    #[test]
    fn exponential_golomb_and_leb128() {
        // uvlc: 0 leading zeros -> 0; "010" -> 1; "011" -> 2.
        let mut r = BitReader::new(&[0b1010_0110]);
        assert_eq!(r.uvlc(), Ok(0));
        assert_eq!(r.uvlc(), Ok(1));
        assert_eq!(r.uvlc(), Ok(2));
        assert_eq!(leb128(&[0xE5, 0x8E, 0x26]), Ok((624_485, 3)));
    }
}
