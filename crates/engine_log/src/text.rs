//! Formatting without allocation: into a fixed buffer, cut at a character boundary.

use core::fmt;
use core::mem::MaybeUninit;

/// A `fmt::Write` into a fixed array on the stack. Text that does not fit is cut at a
/// character boundary and marked with `...`; the contents stay valid UTF-8.
pub struct StackText<const N: usize> {
    bytes: [MaybeUninit<u8>; N],
    len: usize,
    truncated: bool,
}

impl<const N: usize> Default for StackText<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> StackText<N> {
    /// An empty buffer (not zeroed: formatting writes before anything is read).
    #[inline]
    pub fn new() -> Self {
        Self {
            bytes: [MaybeUninit::uninit(); N],
            len: 0,
            truncated: false,
        }
    }

    /// The text written so far.
    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: the first `len` bytes were initialized by `write_str` from whole `str`
        // prefixes cut at character boundaries, so they are valid UTF-8.
        unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                self.bytes.as_ptr().cast::<u8>(),
                self.len,
            ))
        }
    }

    /// Bytes written so far.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing was written.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether text was cut.
    #[inline]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    fn mark_truncated(&mut self) {
        if self.truncated {
            return;
        }
        self.truncated = true;
        const MARK: &[u8] = b"...";
        if N < MARK.len() {
            return;
        }
        let text = self.as_str();
        let mut end = text.len().min(N - MARK.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        for (i, &b) in MARK.iter().enumerate() {
            self.bytes[end + i] = MaybeUninit::new(b);
        }
        self.len = end + MARK.len();
    }
}

impl<const N: usize> fmt::Write for StackText<N> {
    #[inline]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.truncated {
            return Err(fmt::Error);
        }
        let room = N - self.len;
        let mut take = s.len().min(room);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        // SAFETY: `take <= room`, so the copy stays inside `bytes`; source and
        // destination do not overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(
                s.as_ptr(),
                self.bytes.as_mut_ptr().add(self.len).cast::<u8>(),
                take,
            );
        }
        self.len += take;
        if take < s.len() {
            self.mark_truncated();
            return Err(fmt::Error);
        }
        Ok(())
    }
}

/// A `fmt::Write` into a caller's byte slice (crash reports: preallocated, no allocation
/// while writing). Text that does not fit is dropped.
#[derive(Debug)]
pub struct SliceWriter<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl<'a> SliceWriter<'a> {
    /// Writes into `bytes` from the start.
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, len: 0 }
    }

    /// Bytes written.
    pub fn written(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.bytes.len() - self.len;
        let mut take = s.len().min(room);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        if take < s.len() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

/// Writes bytes that are UTF-8 except where a record was cut between parts: invalid
/// sequences become U+FFFD. No allocation.
pub(crate) fn write_lossy(out: &mut dyn fmt::Write, bytes: &[u8]) -> fmt::Result {
    for chunk in bytes.utf8_chunks() {
        out.write_str(chunk.valid())?;
        if !chunk.invalid().is_empty() {
            out.write_char(char::REPLACEMENT_CHARACTER)?;
        }
    }
    Ok(())
}

/// A UTC date and time (`2026-10-01 12:34:56`) for a time since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcTime {
    /// Year.
    pub year: i64,
    /// Month, 1-12.
    pub month: u32,
    /// Day of the month, 1-31.
    pub day: u32,
    /// Hour, 0-23.
    pub hour: u32,
    /// Minute.
    pub minute: u32,
    /// Second.
    pub second: u32,
}

impl UtcTime {
    /// Now.
    pub fn now() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Self::from_unix(secs as i64)
    }

    /// From seconds since 1970-01-01 00:00 UTC (Howard Hinnant's `civil_from_days`).
    pub fn from_unix(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400) as u32;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self {
            year,
            month,
            day,
            hour: rem / 3600,
            minute: rem / 60 % 60,
            second: rem % 60,
        }
    }

    /// `20261001-123456`: sortable, fits in file names.
    pub fn compact(&self) -> StackText<24> {
        let mut out = StackText::new();
        let _ = fmt::write(
            &mut out,
            format_args!(
                "{:04}{:02}{:02}-{:02}{:02}{:02}",
                self.year, self.month, self.day, self.hour, self.minute, self.second
            ),
        );
        out
    }
}

impl fmt::Display for UtcTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write as _;

    #[test]
    fn stack_text_cuts_at_char_boundaries() {
        let mut t = StackText::<8>::new();
        assert!(write!(t, "ab").is_ok());
        assert!(write!(t, "cdeжзи").is_err());
        assert!(t.truncated());
        assert!(t.as_str().ends_with("..."), "{}", t.as_str());
        assert!(t.len() <= 8);
        let mut exact = StackText::<4>::new();
        assert!(exact.write_str("abcd").is_ok());
        assert_eq!(exact.as_str(), "abcd");
    }

    #[test]
    fn utc_dates() {
        assert_eq!(UtcTime::from_unix(0).to_string(), "1970-01-01 00:00:00 UTC");
        // 2026-10-01 12:34:56 UTC.
        let t = UtcTime::from_unix(1_790_858_096);
        assert_eq!(t.to_string(), "2026-10-01 12:34:56 UTC");
        assert_eq!(t.compact().as_str(), "20261001-123456");
        assert_eq!(
            UtcTime::from_unix(951_782_400).to_string(),
            "2000-02-29 00:00:00 UTC"
        );
    }

    #[test]
    fn slice_writer_drops_overflow() {
        let mut buf = [0u8; 5];
        let mut w = SliceWriter::new(&mut buf);
        assert!(w.write_str("hello world").is_err());
        assert_eq!(w.written(), b"hello");
    }
}
