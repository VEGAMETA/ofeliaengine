//! A stream's temporal units into frames: each frame's header parsed against the stream's
//! state and its tiles found in the unit (the AV1 specification's sections 5.3, 5.9.1,
//! 5.10 and 5.11.1), which is what Vulkan Video's AV1 decoding takes.

use super::Av1Error;
use super::bits::BitReader;
use super::frame::{self, FrameHeader, RefState, TileInfo};
use super::obu::{
    OBU_FRAME, OBU_FRAME_HEADER, OBU_REDUNDANT_FRAME_HEADER, OBU_SEQUENCE_HEADER,
    OBU_TEMPORAL_DELIMITER, OBU_TILE_GROUP, Obus, config_obus,
};
use super::sequence::SequenceHeader;

/// A frame of a temporal unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// The frame's header (with the values its parsing derives).
    pub header: FrameHeader,
    /// The frame header OBU's first byte in the temporal unit.
    pub header_obu_offset: u32,
    /// The frame header's first byte (the OBU's payload) in the temporal unit.
    pub header_offset: u32,
    /// The frame's first tile in the unit's tile arrays.
    pub first_tile: u32,
    /// The frame's tiles (none for a frame that shows an existing one).
    pub tile_count: u32,
    /// Which sequence header the frame follows: bumped by every header that differs from
    /// the one before (a new coded video sequence, perhaps of another size).
    pub sequence_serial: u32,
}

/// The frames of a temporal unit and where their tiles are, filled by [`Parser::parse`].
/// Kept from unit to unit, so a stream's steady state does not allocate.
#[derive(Debug, Clone, Default)]
pub struct TemporalUnit {
    /// The unit's frames in decoding order.
    pub frames: Vec<Frame>,
    /// Each tile's first byte in the unit, frame after frame (Vulkan's `pTileOffsets`).
    pub tile_offsets: Vec<u32>,
    /// Each tile's size in bytes (Vulkan's `pTileSizes`).
    pub tile_sizes: Vec<u32>,
}

impl TemporalUnit {
    /// The tiles' offsets of one of the unit's frames.
    pub fn tile_offsets(&self, frame: &Frame) -> &[u32] {
        let first = frame.first_tile as usize;
        &self.tile_offsets[first..first + frame.tile_count as usize]
    }

    /// The tiles' sizes of one of the unit's frames.
    pub fn tile_sizes(&self, frame: &Frame) -> &[u32] {
        let first = frame.first_tile as usize;
        &self.tile_sizes[first..first + frame.tile_count as usize]
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.tile_offsets.clear();
        self.tile_sizes.clear();
    }
}

/// Parses an AV1 stream's temporal units in decoding order, keeping the sequence header
/// and the reference frames' state between them.
#[derive(Debug, Clone, Default)]
pub struct Parser {
    sequence: Option<SequenceHeader>,
    sequence_serial: u32,
    refs: RefState,
}

/// The frame whose tile groups are being read.
#[derive(Debug, Clone, Copy)]
struct Pending {
    frame: usize,
    next_tile: u32,
}

impl Parser {
    /// A parser that has seen no sequence header yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A parser for a WebM track's codec private data (its `av1C` record), whose
    /// configuration OBUs hold the sequence header.
    ///
    /// # Errors
    ///
    /// A record or sequence header that cannot be parsed or is not supported.
    pub fn with_codec_private(private: &[u8]) -> Result<Self, Av1Error> {
        let mut parser = Self::new();
        let mut unit = TemporalUnit::default();
        parser.parse(config_obus(private)?, &mut unit)?;
        Ok(parser)
    }

    /// The stream's current sequence header.
    pub fn sequence(&self) -> Option<&SequenceHeader> {
        self.sequence.as_ref()
    }

    /// The reference frames' state after the frames parsed so far.
    pub fn refs(&self) -> &RefState {
        &self.refs
    }

    /// Forgets the reference frames (for a seek: decoding resumes at a key frame).
    pub fn reset(&mut self) {
        self.refs = RefState::default();
    }

    /// Parses a temporal unit (a WebM block's data) into `unit`, replacing what it held.
    ///
    /// # Errors
    ///
    /// A damaged unit, an unsupported stream, or a frame that needs a reference frame
    /// the parser has not seen (after starting or seeking anywhere but at a key frame).
    /// `unit` then holds the frames parsed before the error.
    pub fn parse(&mut self, data: &[u8], unit: &mut TemporalUnit) -> Result<(), Av1Error> {
        unit.clear();
        if u32::try_from(data.len()).is_err() {
            return Err(Av1Error::Unsupported("a temporal unit of 4 GiB or more"));
        }
        let mut pending: Option<Pending> = None;
        for obu in Obus::new(data) {
            let obu = obu?;
            // drop_obu(): the engine decodes operating point 0 (`choose_operating_point()`).
            if obu.extension
                && obu.kind != OBU_SEQUENCE_HEADER
                && obu.kind != OBU_TEMPORAL_DELIMITER
                && let Some(sequence) = &self.sequence
            {
                let idc = sequence.operating_points[0].idc;
                let in_temporal = idc >> obu.temporal_id & 1 != 0;
                let in_spatial = idc >> (u32::from(obu.spatial_id) + 8) & 1 != 0;
                if idc != 0 && !(in_temporal && in_spatial) {
                    continue;
                }
            }
            match obu.kind {
                OBU_SEQUENCE_HEADER => self.sequence_header(obu.payload)?,
                OBU_TEMPORAL_DELIMITER => {
                    if pending.is_some() {
                        return Err(Av1Error::Corrupt("a frame's tiles are missing"));
                    }
                }
                OBU_FRAME_HEADER | OBU_REDUNDANT_FRAME_HEADER | OBU_FRAME => {
                    if pending.is_some() {
                        if obu.kind == OBU_FRAME {
                            return Err(Av1Error::Corrupt("a frame's tiles are missing"));
                        }
                        // frame_header_copy(): a repeat of the frame's header.
                        continue;
                    }
                    if obu.kind == OBU_REDUNDANT_FRAME_HEADER {
                        // A copy whose frame is complete (or was lost): nothing to read.
                        continue;
                    }
                    let sequence = self.sequence.as_ref().ok_or(Av1Error::MissingReference)?;
                    let mut r = BitReader::new(obu.payload);
                    let header = frame::parse(
                        &mut r,
                        sequence,
                        &self.refs,
                        obu.temporal_id,
                        obu.spatial_id,
                    )?;
                    if header.show_existing_frame && obu.kind == OBU_FRAME {
                        return Err(Av1Error::Corrupt("a frame OBU showing an existing frame"));
                    }
                    let index = unit.frames.len();
                    unit.frames.push(Frame {
                        header,
                        header_obu_offset: obu.offset as u32,
                        header_offset: obu.payload_offset as u32,
                        first_tile: unit.tile_offsets.len() as u32,
                        tile_count: 0,
                        sequence_serial: self.sequence_serial,
                    });
                    // The reference frame update process (7.20) runs once the frame is
                    // decoded; nothing parsed before then reads the references.
                    self.refs.update(&header, sequence);
                    if header.show_existing_frame {
                        continue;
                    }
                    let mut current = Pending {
                        frame: index,
                        next_tile: 0,
                    };
                    if obu.kind == OBU_FRAME {
                        // frame_obu(): the first tile group follows the byte-aligned header.
                        r.byte_alignment();
                        let at = r.byte_position();
                        let payload = obu.payload.get(at..).ok_or(Av1Error::Truncated)?;
                        tile_group(
                            payload,
                            obu.payload_offset + at,
                            &header.tile_info,
                            unit,
                            &mut current,
                        )?;
                    }
                    pending = (current.next_tile < header.tile_info.count()).then_some(current);
                }
                OBU_TILE_GROUP => {
                    let Some(mut current) = pending else {
                        return Err(Av1Error::Corrupt("a tile group without its frame header"));
                    };
                    let tiles = unit.frames[current.frame].header.tile_info;
                    tile_group(obu.payload, obu.payload_offset, &tiles, unit, &mut current)?;
                    pending = (current.next_tile < tiles.count()).then_some(current);
                }
                // Metadata, padding, tile lists (large scale tile decoding) and reserved
                // types carry nothing to decode.
                _ => {}
            }
        }
        if pending.is_some() {
            return Err(Av1Error::Corrupt("a frame's tiles are missing"));
        }
        Ok(())
    }

    fn sequence_header(&mut self, payload: &[u8]) -> Result<(), Av1Error> {
        let sequence = SequenceHeader::parse(payload)?;
        let spatial_layers = (sequence.operating_points[0].idc >> 8).count_ones();
        if spatial_layers > 1 {
            return Err(Av1Error::Unsupported("spatial layers"));
        }
        if self.sequence.as_ref() != Some(&sequence) {
            self.sequence = Some(sequence);
            self.sequence_serial += 1;
        }
        Ok(())
    }
}

/// `tile_group_obu()`: finds the group's tiles, the next ones of `current`'s frame.
fn tile_group(
    payload: &[u8],
    payload_offset: usize,
    tiles: &TileInfo,
    unit: &mut TemporalUnit,
    current: &mut Pending,
) -> Result<(), Av1Error> {
    let num_tiles = tiles.count();
    let mut r = BitReader::new(payload);
    let start_and_end_present = num_tiles > 1 && r.flag()?;
    let (start, end) = if start_and_end_present {
        let bits = tiles.tile_cols_log2 + tiles.tile_rows_log2;
        (r.f(bits)?, r.f(bits)?)
    } else {
        (0, num_tiles - 1)
    };
    if start != current.next_tile || end < start || end >= num_tiles {
        return Err(Av1Error::Corrupt("tile groups out of order"));
    }
    r.byte_alignment();
    let mut at = r.byte_position();
    let size_bytes = tiles.size_bytes() as usize;
    for tile in start..=end {
        let size = if tile == end {
            payload.len().checked_sub(at).ok_or(Av1Error::Truncated)?
        } else {
            // tile_size_minus_1: le(TileSizeBytes).
            let bytes = payload
                .get(at..at + size_bytes)
                .ok_or(Av1Error::Truncated)?;
            at += size_bytes;
            let minus_1 = bytes
                .iter()
                .rev()
                .fold(0usize, |value, &byte| value << 8 | usize::from(byte));
            minus_1 + 1
        };
        if size == 0 || at + size > payload.len() {
            return Err(Av1Error::Truncated);
        }
        unit.tile_offsets.push((payload_offset + at) as u32);
        unit.tile_sizes.push(size as u32);
        at += size;
    }
    unit.frames[current.frame].tile_count += end - start + 1;
    current.next_tile = end + 1;
    Ok(())
}
