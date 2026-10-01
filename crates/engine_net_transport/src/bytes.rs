//! Bounds-checked little-endian readers and writers over byte slices. Every parse error
//! is a `None`: malformed input from the network is dropped, never a panic.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Writes into a fixed slice.
pub(crate) struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn bytes(&mut self, data: &[u8]) -> Option<()> {
        let end = self.pos.checked_add(data.len())?;
        self.buf.get_mut(self.pos..end)?.copy_from_slice(data);
        self.pos = end;
        Some(())
    }

    pub(crate) fn u8(&mut self, v: u8) -> Option<()> {
        self.bytes(&[v])
    }

    pub(crate) fn u16(&mut self, v: u16) -> Option<()> {
        self.bytes(&v.to_le_bytes())
    }

    pub(crate) fn u32(&mut self, v: u32) -> Option<()> {
        self.bytes(&v.to_le_bytes())
    }

    pub(crate) fn u64(&mut self, v: u64) -> Option<()> {
        self.bytes(&v.to_le_bytes())
    }

    /// LEB128, at most 5 bytes.
    pub(crate) fn varint(&mut self, mut v: u32) -> Option<()> {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                return self.u8(byte);
            }
            self.u8(byte | 0x80)?;
        }
    }

    /// A part already written, for filling in place.
    pub(crate) fn slice_mut(&mut self, start: usize, len: usize) -> &mut [u8] {
        &mut self.buf[start..start + len]
    }

    pub(crate) fn zeros(&mut self, n: usize) -> Option<()> {
        let end = self.pos.checked_add(n)?;
        self.buf.get_mut(self.pos..end)?.fill(0);
        self.pos = end;
        Some(())
    }

    /// Family byte (4 or 6), address, port.
    pub(crate) fn addr(&mut self, addr: SocketAddr) -> Option<()> {
        match addr.ip() {
            IpAddr::V4(ip) => {
                self.u8(4)?;
                self.bytes(&ip.octets())?;
            }
            IpAddr::V6(ip) => {
                self.u8(6)?;
                self.bytes(&ip.octets())?;
            }
        }
        self.u16(addr.port())
    }
}

/// Reads from a slice.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.bytes(N)?.try_into().ok()
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn varint(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for shift in (0..35).step_by(7) {
            let byte = self.u8()?;
            let bits = u32::from(byte & 0x7f);
            if shift == 28 && bits > 0x0f {
                return None;
            }
            v |= bits << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    pub(crate) fn addr(&mut self) -> Option<SocketAddr> {
        let ip = match self.u8()? {
            4 => IpAddr::V4(Ipv4Addr::from(self.array::<4>()?)),
            6 => IpAddr::V6(Ipv6Addr::from(self.array::<16>()?)),
            _ => return None,
        };
        Some(SocketAddr::new(ip, self.u16()?))
    }
}

/// Bytes `Writer::varint` writes for `v`.
pub(crate) fn varint_len(v: u32) -> usize {
    match v {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        0x4000..=0x1f_ffff => 3,
        0x20_0000..=0xfff_ffff => 4,
        _ => 5,
    }
}

/// An IPv4-mapped IPv6 address (from a dual-stack socket) as plain IPv4.
pub(crate) fn normalize_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn varints_round_trip() {
        let mut buf = [0u8; 64];
        let values = [
            0u32,
            1,
            127,
            128,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            u32::MAX,
        ];
        let mut w = Writer::new(&mut buf);
        for &v in &values {
            let before = w.pos();
            w.varint(v).unwrap();
            assert_eq!(w.pos() - before, varint_len(v));
        }
        let mut r = Reader::new(&buf);
        for &v in &values {
            assert_eq!(r.varint(), Some(v));
        }
        // Overlong encodings are rejected.
        assert_eq!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x7f]).varint(), None);
        assert_eq!(Reader::new(&[0x80]).varint(), None);
    }

    #[test]
    fn addresses_round_trip() {
        let mut buf = [0u8; 64];
        let a: SocketAddr = "192.168.1.20:40000".parse().unwrap();
        let b: SocketAddr = "[2001:db8::1]:27015".parse().unwrap();
        let mut w = Writer::new(&mut buf);
        w.addr(a).unwrap();
        w.addr(b).unwrap();
        let mut r = Reader::new(&buf);
        assert_eq!(r.addr(), Some(a));
        assert_eq!(r.addr(), Some(b));
        let mapped: SocketAddr = "[::ffff:10.0.0.2]:5".parse().unwrap();
        assert_eq!(normalize_addr(mapped), "10.0.0.2:5".parse().unwrap());
    }
}
