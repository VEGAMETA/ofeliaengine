//! Little-endian serialization helpers for the cooked formats (skeletons, clips).

use crate::FormatError;

/// Appends little-endian values to a byte vector.
#[derive(Debug, Default)]
pub(crate) struct Writer {
    pub(crate) bytes: Vec<u8>,
}

impl Writer {
    pub(crate) fn u8(&mut self, v: u8) {
        self.bytes.push(v);
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn f32(&mut self, v: f32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn f32s(&mut self, v: &[f32]) {
        for &x in v {
            self.f32(x);
        }
    }
    pub(crate) fn str(&mut self, s: &str) {
        let s = &s.as_bytes()[..s.len().min(u16::MAX as usize)];
        self.u16(s.len() as u16);
        self.bytes.extend_from_slice(s);
    }
    pub(crate) fn raw(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
    }
}

/// Reads little-endian values, failing on truncation.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    pub(crate) bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        if n > self.bytes.len() {
            return Err(FormatError("truncated"));
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], FormatError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.array::<1>()?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub(crate) fn f32(&mut self) -> Result<f32, FormatError> {
        let v = f32::from_le_bytes(self.array()?);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(FormatError("non-finite number"))
        }
    }
    pub(crate) fn f32s<const N: usize>(&mut self) -> Result<[f32; N], FormatError> {
        let mut out = [0.0; N];
        for v in &mut out {
            *v = self.f32()?;
        }
        Ok(out)
    }
    pub(crate) fn str(&mut self) -> Result<String, FormatError> {
        let n = self.u16()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| FormatError("bad name"))
    }
    pub(crate) fn finish(&self) -> Result<(), FormatError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(FormatError("trailing bytes"))
        }
    }
}
