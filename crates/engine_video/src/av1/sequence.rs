//! The sequence header OBU (the AV1 specification's sections 5.5 and 6.4). Fields named as
//! in the specification are its syntax elements.

use super::Av1Error;
use super::bits::BitReader;

/// `seq_force_screen_content_tools` and `seq_force_integer_mv`: the frame header chooses.
pub const SELECT: u8 = 2;

/// Operating points a sequence header may list.
pub const MAX_OPERATING_POINTS: usize = 32;

/// `timing_info()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct TimingInfo {
    pub num_units_in_display_tick: u32,
    pub time_scale: u32,
    pub equal_picture_interval: bool,
    pub num_ticks_per_picture_minus_1: u32,
}

/// `decoder_model_info()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct DecoderModelInfo {
    pub buffer_delay_length_minus_1: u8,
    pub num_units_in_decoding_tick: u32,
    pub buffer_removal_time_length_minus_1: u8,
    pub frame_presentation_time_length_minus_1: u8,
}

/// An operating point of the sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct OperatingPoint {
    pub idc: u16,
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    pub decoder_model_present: bool,
    pub initial_display_delay_present: bool,
    pub initial_display_delay_minus_1: u8,
}

/// `color_config()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct ColorConfig {
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    /// 8, 10 or 12.
    pub bit_depth: u8,
    pub mono_chrome: bool,
    pub color_description_present_flag: bool,
    /// The colour primaries (1: BT.709, 9: BT.2020; 2: unspecified).
    pub color_primaries: u8,
    /// The transfer characteristics (1: BT.709, 13: sRGB, 16: PQ, 18: HLG).
    pub transfer_characteristics: u8,
    /// The matrix coefficients (1: BT.709, 6: BT.601, 9: BT.2020; 0: identity, RGB).
    pub matrix_coefficients: u8,
    /// Full range (0..255) when set, studio range (16..235) otherwise.
    pub color_range: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub separate_uv_delta_q: bool,
}

impl ColorConfig {
    /// `NumPlanes`.
    pub fn num_planes(&self) -> u32 {
        if self.mono_chrome { 1 } else { 3 }
    }
}

/// A parsed sequence header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    pub still_picture: bool,
    pub reduced_still_picture_header: bool,
    pub timing_info: Option<TimingInfo>,
    pub decoder_model_info: Option<DecoderModelInfo>,
    pub initial_display_delay_present_flag: bool,
    pub operating_points: [OperatingPoint; MAX_OPERATING_POINTS],
    pub operating_points_count: usize,
    pub frame_width_bits_minus_1: u8,
    pub frame_height_bits_minus_1: u8,
    pub max_frame_width_minus_1: u32,
    pub max_frame_height_minus_1: u32,
    pub frame_id_numbers_present_flag: bool,
    pub delta_frame_id_length_minus_2: u8,
    pub additional_frame_id_length_minus_1: u8,
    pub use_128x128_superblock: bool,
    pub enable_filter_intra: bool,
    pub enable_intra_edge_filter: bool,
    pub enable_interintra_compound: bool,
    pub enable_masked_compound: bool,
    pub enable_warped_motion: bool,
    pub enable_dual_filter: bool,
    pub enable_order_hint: bool,
    pub enable_jnt_comp: bool,
    pub enable_ref_frame_mvs: bool,
    pub seq_choose_screen_content_tools: bool,
    /// 0, 1 or [`SELECT`].
    pub seq_force_screen_content_tools: u8,
    pub seq_choose_integer_mv: bool,
    /// 0, 1 or [`SELECT`].
    pub seq_force_integer_mv: u8,
    pub order_hint_bits_minus_1: u8,
    /// `OrderHintBits` (0 without order hints).
    pub order_hint_bits: u32,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub color: ColorConfig,
    pub film_grain_params_present: bool,
}

impl SequenceHeader {
    /// Parses a sequence header OBU's payload.
    ///
    /// # Errors
    ///
    /// A truncated or invalid header.
    pub fn parse(data: &[u8]) -> Result<Self, Av1Error> {
        let mut r = BitReader::new(data);
        let mut s = Self {
            seq_profile: r.f(3)? as u8,
            still_picture: r.flag()?,
            reduced_still_picture_header: r.flag()?,
            ..Self::default()
        };
        if s.seq_profile > 2 {
            return Err(Av1Error::Unsupported("sequence profile above 2"));
        }
        if s.reduced_still_picture_header {
            s.operating_points[0].seq_level_idx = r.f(5)? as u8;
            s.operating_points_count = 1;
        } else {
            if r.flag()? {
                let mut t = TimingInfo {
                    num_units_in_display_tick: r.f(32)?,
                    time_scale: r.f(32)?,
                    equal_picture_interval: r.flag()?,
                    num_ticks_per_picture_minus_1: 0,
                };
                if t.equal_picture_interval {
                    t.num_ticks_per_picture_minus_1 = r.uvlc()?;
                }
                s.timing_info = Some(t);
                if r.flag()? {
                    s.decoder_model_info = Some(DecoderModelInfo {
                        buffer_delay_length_minus_1: r.f(5)? as u8,
                        num_units_in_decoding_tick: r.f(32)?,
                        buffer_removal_time_length_minus_1: r.f(5)? as u8,
                        frame_presentation_time_length_minus_1: r.f(5)? as u8,
                    });
                }
            }
            s.initial_display_delay_present_flag = r.flag()?;
            s.operating_points_count = r.f(5)? as usize + 1;
            for i in 0..s.operating_points_count {
                let mut op = OperatingPoint {
                    idc: r.f(12)? as u16,
                    seq_level_idx: r.f(5)? as u8,
                    ..OperatingPoint::default()
                };
                if op.seq_level_idx > 7 {
                    op.seq_tier = r.f(1)? as u8;
                }
                if let Some(model) = s.decoder_model_info {
                    op.decoder_model_present = r.flag()?;
                    if op.decoder_model_present {
                        // operating_parameters_info(): decoder and encoder buffer delays, low
                        // delay mode.
                        let n = u32::from(model.buffer_delay_length_minus_1) + 1;
                        r.f(n)?;
                        r.f(n)?;
                        r.f(1)?;
                    }
                }
                if s.initial_display_delay_present_flag {
                    op.initial_display_delay_present = r.flag()?;
                    if op.initial_display_delay_present {
                        op.initial_display_delay_minus_1 = r.f(4)? as u8;
                    }
                }
                s.operating_points[i] = op;
            }
        }
        s.frame_width_bits_minus_1 = r.f(4)? as u8;
        s.frame_height_bits_minus_1 = r.f(4)? as u8;
        s.max_frame_width_minus_1 = r.f(u32::from(s.frame_width_bits_minus_1) + 1)?;
        s.max_frame_height_minus_1 = r.f(u32::from(s.frame_height_bits_minus_1) + 1)?;
        if !s.reduced_still_picture_header {
            s.frame_id_numbers_present_flag = r.flag()?;
        }
        if s.frame_id_numbers_present_flag {
            s.delta_frame_id_length_minus_2 = r.f(4)? as u8;
            s.additional_frame_id_length_minus_1 = r.f(3)? as u8;
        }
        s.use_128x128_superblock = r.flag()?;
        s.enable_filter_intra = r.flag()?;
        s.enable_intra_edge_filter = r.flag()?;
        if s.reduced_still_picture_header {
            s.seq_force_screen_content_tools = SELECT;
            s.seq_force_integer_mv = SELECT;
        } else {
            s.enable_interintra_compound = r.flag()?;
            s.enable_masked_compound = r.flag()?;
            s.enable_warped_motion = r.flag()?;
            s.enable_dual_filter = r.flag()?;
            s.enable_order_hint = r.flag()?;
            if s.enable_order_hint {
                s.enable_jnt_comp = r.flag()?;
                s.enable_ref_frame_mvs = r.flag()?;
            }
            s.seq_choose_screen_content_tools = r.flag()?;
            s.seq_force_screen_content_tools = if s.seq_choose_screen_content_tools {
                SELECT
            } else {
                r.f(1)? as u8
            };
            if s.seq_force_screen_content_tools > 0 {
                s.seq_choose_integer_mv = r.flag()?;
                s.seq_force_integer_mv = if s.seq_choose_integer_mv {
                    SELECT
                } else {
                    r.f(1)? as u8
                };
            } else {
                s.seq_force_integer_mv = SELECT;
            }
            if s.enable_order_hint {
                s.order_hint_bits_minus_1 = r.f(3)? as u8;
                s.order_hint_bits = u32::from(s.order_hint_bits_minus_1) + 1;
            }
        }
        s.enable_superres = r.flag()?;
        s.enable_cdef = r.flag()?;
        s.enable_restoration = r.flag()?;
        s.color = color_config(&mut r, s.seq_profile)?;
        s.film_grain_params_present = r.flag()?;
        Ok(s)
    }

    /// The upper limit of `frame_width_minus_1 + 1`.
    pub fn max_width(&self) -> u32 {
        self.max_frame_width_minus_1 + 1
    }

    /// The upper limit of `frame_height_minus_1 + 1`.
    pub fn max_height(&self) -> u32 {
        self.max_frame_height_minus_1 + 1
    }

    /// `idLen` of frame ids, when they are present.
    pub fn frame_id_length(&self) -> u32 {
        u32::from(self.additional_frame_id_length_minus_1)
            + u32::from(self.delta_frame_id_length_minus_2)
            + 3
    }
}

fn color_config(r: &mut BitReader<'_>, profile: u8) -> Result<ColorConfig, Av1Error> {
    let mut c = ColorConfig {
        high_bitdepth: r.flag()?,
        ..ColorConfig::default()
    };
    if profile == 2 && c.high_bitdepth {
        c.twelve_bit = r.flag()?;
        c.bit_depth = if c.twelve_bit { 12 } else { 10 };
    } else {
        c.bit_depth = if c.high_bitdepth { 10 } else { 8 };
    }
    c.mono_chrome = if profile == 1 { false } else { r.flag()? };
    c.color_description_present_flag = r.flag()?;
    if c.color_description_present_flag {
        c.color_primaries = r.f(8)? as u8;
        c.transfer_characteristics = r.f(8)? as u8;
        c.matrix_coefficients = r.f(8)? as u8;
    } else {
        c.color_primaries = 2;
        c.transfer_characteristics = 2;
        c.matrix_coefficients = 2;
    }
    if c.mono_chrome {
        c.color_range = r.flag()?;
        c.subsampling_x = true;
        c.subsampling_y = true;
        // CSP_UNKNOWN.
        c.chroma_sample_position = 0;
        c.separate_uv_delta_q = false;
        return Ok(c);
    } else if c.color_primaries == 1
        && c.transfer_characteristics == 13
        && c.matrix_coefficients == 0
    {
        c.color_range = true;
        c.subsampling_x = false;
        c.subsampling_y = false;
    } else {
        c.color_range = r.flag()?;
        if profile == 0 {
            c.subsampling_x = true;
            c.subsampling_y = true;
        } else if profile == 1 {
            c.subsampling_x = false;
            c.subsampling_y = false;
        } else if c.bit_depth == 12 {
            c.subsampling_x = r.flag()?;
            c.subsampling_y = if c.subsampling_x { r.flag()? } else { false };
        } else {
            c.subsampling_x = true;
            c.subsampling_y = false;
        }
        if c.subsampling_x && c.subsampling_y {
            c.chroma_sample_position = r.f(2)? as u8;
        }
    }
    c.separate_uv_delta_q = r.flag()?;
    Ok(c)
}
