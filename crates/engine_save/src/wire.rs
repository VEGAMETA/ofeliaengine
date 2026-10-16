//! The binary's primitives: LEB128 lengths, strings, and a reader that checks every
//! length against what is left (a save is untrusted input).

use crate::SaveError;

pub(crate) fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub(crate) fn write_str(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// Reads a save's bytes, never past their end.
#[derive(Debug, Clone)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.at
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.at
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], SaveError> {
        if n > self.remaining() {
            return Err(SaveError::damaged("the data ends early"));
        }
        let b = &self.data[self.at..self.at + n];
        self.at += n;
        Ok(b)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, SaveError> {
        Ok(self.bytes(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, SaveError> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u128(&mut self) -> Result<u128, SaveError> {
        let mut v = [0u8; 16];
        v.copy_from_slice(self.bytes(16)?);
        Ok(u128::from_le_bytes(v))
    }

    pub(crate) fn varint(&mut self) -> Result<u64, SaveError> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(SaveError::damaged("a number too long"))
    }

    /// A count of things that take at least a byte each: never more than what is left.
    pub(crate) fn len(&mut self) -> Result<usize, SaveError> {
        let n = self.varint()?;
        usize::try_from(n)
            .ok()
            .filter(|&n| n <= self.remaining())
            .ok_or_else(|| SaveError::damaged("a length past the end"))
    }

    pub(crate) fn string(&mut self) -> Result<&'a str, SaveError> {
        let n = self.len()?;
        core::str::from_utf8(self.bytes(n)?)
            .map_err(|_| SaveError::damaged("text that is not UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_strings_round_trip_and_never_read_past_the_end() {
        let mut out = Vec::new();
        for v in [0u64, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            write_varint(&mut out, v);
        }
        write_str(&mut out, "Офелия");
        let mut r = Reader::new(&out);
        for v in [0u64, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(r.varint().ok(), Some(v));
        }
        assert_eq!(r.string().ok(), Some("Офелия"));
        assert_eq!(r.remaining(), 0);
        assert!(r.u8().is_err());
        // A length past the end is refused before anything is read.
        let mut r = Reader::new(&[0x10, b'a']);
        assert!(r.string().is_err());
    }
}
