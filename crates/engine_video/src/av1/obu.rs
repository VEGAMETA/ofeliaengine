//! Open bitstream units (the AV1 specification's section 5.3) in the low overhead
//! bitstream format a WebM block holds, and the `av1C` record of a track's codec private
//! data.

use super::Av1Error;
use super::bits::leb128;

/// `OBU_SEQUENCE_HEADER`.
pub const OBU_SEQUENCE_HEADER: u8 = 1;
/// `OBU_TEMPORAL_DELIMITER`.
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
/// `OBU_FRAME_HEADER`.
pub const OBU_FRAME_HEADER: u8 = 3;
/// `OBU_TILE_GROUP`.
pub const OBU_TILE_GROUP: u8 = 4;
/// `OBU_METADATA`.
pub const OBU_METADATA: u8 = 5;
/// `OBU_FRAME`: a frame header and its first tile group.
pub const OBU_FRAME: u8 = 6;
/// `OBU_REDUNDANT_FRAME_HEADER`.
pub const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;
/// `OBU_TILE_LIST` (large scale tile decoding only).
pub const OBU_TILE_LIST: u8 = 8;
/// `OBU_PADDING`.
pub const OBU_PADDING: u8 = 15;

/// One open bitstream unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    /// `obu_type`.
    pub kind: u8,
    /// Whether the OBU has its extension header (its layer is then known).
    pub extension: bool,
    /// `temporal_id`, 0 without the extension.
    pub temporal_id: u8,
    /// `spatial_id`, 0 without the extension.
    pub spatial_id: u8,
    /// The OBU's first byte (its header) in the data the OBUs were read from.
    pub offset: usize,
    /// The payload's first byte in the data the OBUs were read from.
    pub payload_offset: usize,
    /// The payload (`obu_size` bytes).
    pub payload: &'a [u8],
}

/// The OBUs of a temporal unit (or of `av1C`'s configuration OBUs), in order.
#[derive(Debug, Clone)]
pub struct Obus<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Obus<'a> {
    /// The OBUs in `data`. Each has its size field, except perhaps the last one, which
    /// then fills the rest of the data (as the WebM and ISOBMFF mappings allow).
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
}

impl<'a> Iterator for Obus<'a> {
    type Item = Result<Obu<'a>, Av1Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.data.len() {
            return None;
        }
        let result = self.read();
        if result.is_err() {
            // Nothing after a damaged OBU can be found.
            self.position = self.data.len();
        }
        Some(result)
    }
}

impl<'a> Obus<'a> {
    fn read(&mut self) -> Result<Obu<'a>, Av1Error> {
        let offset = self.position;
        let rest = &self.data[offset..];
        let header = rest[0];
        if header & 0x80 != 0 {
            return Err(Av1Error::Corrupt("OBU forbidden bit set"));
        }
        let kind = header >> 3 & 0xF;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut at = 1;
        let (mut temporal_id, mut spatial_id) = (0, 0);
        if extension {
            let byte = *rest.get(1).ok_or(Av1Error::Truncated)?;
            temporal_id = byte >> 5;
            spatial_id = byte >> 3 & 3;
            at = 2;
        }
        let size = if has_size {
            let (size, length) = leb128(&rest[at..])?;
            at += length;
            usize::try_from(size).map_err(|_| Av1Error::Corrupt("OBU size"))?
        } else {
            rest.len() - at
        };
        let end = at.checked_add(size).ok_or(Av1Error::Truncated)?;
        let payload = rest.get(at..end).ok_or(Av1Error::Truncated)?;
        self.position = offset + end;
        Ok(Obu {
            kind,
            extension,
            temporal_id,
            spatial_id,
            offset,
            payload_offset: offset + at,
            payload,
        })
    }
}

/// The configuration OBUs of an `av1C` record (a WebM track's codec private data, the
/// AV1 codec ISOBMFF binding's `AV1CodecConfigurationRecord`): the sequence header OBU,
/// when the record has it.
///
/// # Errors
///
/// Data that is not a version 1 `av1C` record.
pub fn config_obus(private: &[u8]) -> Result<&[u8], Av1Error> {
    // marker (1) and version (1) in the first byte; the profile, level, tier, depth,
    // subsampling and presentation delay in the next three.
    match private.first() {
        Some(0x81) if private.len() >= 4 => Ok(&private[4..]),
        Some(_) => Err(Av1Error::Corrupt("not a version 1 av1C record")),
        None => Err(Av1Error::Truncated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obus_with_and_without_size_fields() {
        let data = [
            // A temporal delimiter with an empty payload.
            0x12,
            0x00,
            // A padding OBU with the extension (temporal 5, spatial 2) and two bytes.
            0x7E,
            0b1011_0000,
            0x02,
            0xAA,
            0xBB,
            // A last OBU (a frame) without a size field: the rest of the data.
            0x30,
            1,
            2,
            3,
        ];
        let obus: Vec<_> = Obus::new(&data).collect::<Result<_, _>>().unwrap();
        assert_eq!(obus.len(), 3);
        assert_eq!(obus[0].kind, OBU_TEMPORAL_DELIMITER);
        assert!(obus[0].payload.is_empty());
        assert_eq!(obus[1].kind, OBU_PADDING);
        assert_eq!((obus[1].temporal_id, obus[1].spatial_id), (5, 2));
        assert_eq!(obus[1].payload, &[0xAA, 0xBB]);
        assert_eq!(obus[1].payload_offset, 5);
        assert_eq!(obus[2].kind, OBU_FRAME);
        assert_eq!(obus[2].payload, &[1, 2, 3]);
    }

    #[test]
    fn damaged_obus_end_the_iteration() {
        // A size beyond the data.
        let data = [0x12, 0x05, 0x00];
        let mut obus = Obus::new(&data);
        assert_eq!(obus.next(), Some(Err(Av1Error::Truncated)));
        assert_eq!(obus.next(), None);
        assert_eq!(config_obus(&[0x81, 0, 0, 0, 0x0A]), Ok(&[0x0A][..]));
        assert!(config_obus(&[0x01, 0, 0, 0]).is_err());
    }
}
