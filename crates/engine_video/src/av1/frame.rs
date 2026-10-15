//! The frame header OBU (the AV1 specification's sections 5.9 and 6.8) and the reference
//! frames' state its parsing reads and its reference frame update process writes
//! (sections 7.20 and 7.21).

use super::bits::BitReader;
use super::sequence::{SELECT, SequenceHeader};
use super::{
    Av1Error, MAX_SEGMENTS, MAX_TILE_COLS, MAX_TILE_ROWS, NUM_REF_FRAMES, PRIMARY_REF_NONE,
    REFS_PER_FRAME, SEG_LVL_MAX, TOTAL_REFS_PER_FRAME,
};

/// `frame_type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FrameType {
    /// `KEY_FRAME`: intra coded; shown, it resets every reference.
    #[default]
    Key = 0,
    /// `INTER_FRAME`.
    Inter = 1,
    /// `INTRA_ONLY_FRAME`: intra coded, keeping the references.
    IntraOnly = 2,
    /// `S_FRAME`: an inter frame that refreshes every reference.
    Switch = 3,
}

impl FrameType {
    fn from_bits(bits: u32) -> Self {
        match bits {
            0 => Self::Key,
            1 => Self::Inter,
            2 => Self::IntraOnly,
            _ => Self::Switch,
        }
    }

    /// `FrameIsIntra`.
    pub fn is_intra(self) -> bool {
        matches!(self, Self::Key | Self::IntraOnly)
    }
}

/// `INTRA_FRAME`: the frame itself, as reference frames are numbered.
pub const INTRA_FRAME: usize = 0;
/// `LAST_FRAME`, the first of the seven references.
pub const LAST_FRAME: usize = 1;
/// `LAST2_FRAME`.
pub const LAST2_FRAME: usize = 2;
/// `LAST3_FRAME`.
pub const LAST3_FRAME: usize = 3;
/// `GOLDEN_FRAME`.
pub const GOLDEN_FRAME: usize = 4;
/// `BWDREF_FRAME`.
pub const BWDREF_FRAME: usize = 5;
/// `ALTREF2_FRAME`.
pub const ALTREF2_FRAME: usize = 6;
/// `ALTREF_FRAME`, the last of the seven references.
pub const ALTREF_FRAME: usize = 7;

/// `WARPEDMODEL_PREC_BITS`.
const WARPEDMODEL_PREC_BITS: u32 = 16;

/// Global motion `IDENTITY`: no global motion.
pub const IDENTITY: u8 = 0;
/// Global motion `TRANSLATION`.
pub const TRANSLATION: u8 = 1;
/// Global motion `ROTZOOM`: rotation and zoom.
pub const ROTZOOM: u8 = 2;
/// Global motion `AFFINE`.
pub const AFFINE: u8 = 3;

/// `tile_info()`. Fields named as in the specification are its syntax elements and
/// variables, here and in the other headers' structures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct TileInfo {
    pub uniform_tile_spacing_flag: bool,
    pub tile_cols: u32,
    pub tile_rows: u32,
    pub tile_cols_log2: u32,
    pub tile_rows_log2: u32,
    /// `MiColStarts`, `TileCols + 1` of them.
    pub mi_col_starts: [u16; MAX_TILE_COLS + 1],
    /// `MiRowStarts`, `TileRows + 1` of them.
    pub mi_row_starts: [u16; MAX_TILE_ROWS + 1],
    /// Each tile column's width in superblocks, minus 1 (uniform spacing too).
    pub width_in_sbs_minus_1: [u16; MAX_TILE_COLS],
    /// Each tile row's height in superblocks, minus 1.
    pub height_in_sbs_minus_1: [u16; MAX_TILE_ROWS],
    pub context_update_tile_id: u32,
    pub tile_size_bytes_minus_1: u8,
}

impl Default for TileInfo {
    fn default() -> Self {
        Self {
            uniform_tile_spacing_flag: false,
            tile_cols: 1,
            tile_rows: 1,
            tile_cols_log2: 0,
            tile_rows_log2: 0,
            mi_col_starts: [0; MAX_TILE_COLS + 1],
            mi_row_starts: [0; MAX_TILE_ROWS + 1],
            width_in_sbs_minus_1: [0; MAX_TILE_COLS],
            height_in_sbs_minus_1: [0; MAX_TILE_ROWS],
            context_update_tile_id: 0,
            tile_size_bytes_minus_1: 0,
        }
    }
}

impl TileInfo {
    /// `NumTiles`.
    pub fn count(&self) -> u32 {
        self.tile_cols * self.tile_rows
    }

    /// `TileSizeBytes`.
    pub fn size_bytes(&self) -> u32 {
        u32::from(self.tile_size_bytes_minus_1) + 1
    }
}

/// `quantization_params()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Quantization {
    pub base_q_idx: u8,
    pub delta_q_y_dc: i8,
    pub delta_q_u_dc: i8,
    pub delta_q_u_ac: i8,
    pub delta_q_v_dc: i8,
    pub delta_q_v_ac: i8,
    pub diff_uv_delta: bool,
    pub using_qmatrix: bool,
    pub qm_y: u8,
    pub qm_u: u8,
    pub qm_v: u8,
}

/// `segmentation_params()`, with the features in force (loaded from the primary reference
/// frame when not updated).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Segmentation {
    pub enabled: bool,
    pub update_map: bool,
    pub temporal_update: bool,
    pub update_data: bool,
    pub feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    pub feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
    pub seg_id_pre_skip: bool,
    pub last_active_seg_id: u8,
}

/// `delta_q_params()` and `delta_lf_params()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Deltas {
    pub delta_q_present: bool,
    pub delta_q_res: u8,
    pub delta_lf_present: bool,
    pub delta_lf_res: u8,
    pub delta_lf_multi: bool,
}

/// `loop_filter_params()`, with the deltas in force.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct LoopFilter {
    pub level: [u8; 4],
    pub sharpness: u8,
    pub delta_enabled: bool,
    pub delta_update: bool,
    /// Bit `i`: `update_ref_delta` for reference frame `i`.
    pub update_ref_delta: u8,
    pub ref_deltas: [i8; TOTAL_REFS_PER_FRAME],
    /// Bit `i`: `update_mode_delta` for mode `i`.
    pub update_mode_delta: u8,
    pub mode_deltas: [i8; 2],
}

/// `cdef_params()`: the strengths as coded (a secondary strength of 3 means 4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Cdef {
    pub damping_minus_3: u8,
    pub bits: u8,
    pub y_pri_strength: [u8; 8],
    pub y_sec_strength: [u8; 8],
    pub uv_pri_strength: [u8; 8],
    pub uv_sec_strength: [u8; 8],
}

/// `lr_params()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Restoration {
    /// `FrameRestorationType` per plane: 0 none, 1 Wiener, 2 self-guided, 3 switchable.
    pub frame_restoration_type: [u8; 3],
    pub lr_unit_shift: u8,
    pub lr_uv_shift: u8,
    /// `LoopRestorationSize` per plane (in luma samples for plane 0).
    pub loop_restoration_size: [u16; 3],
    pub uses_lr: bool,
    pub uses_chroma_lr: bool,
}

/// `global_motion_params()`, per reference frame (`[0]`, the intra frame, unused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct GlobalMotion {
    pub gm_type: [u8; TOTAL_REFS_PER_FRAME],
    pub gm_params: [[i32; 6]; TOTAL_REFS_PER_FRAME],
}

impl Default for GlobalMotion {
    fn default() -> Self {
        let identity = [
            0,
            0,
            1 << WARPEDMODEL_PREC_BITS,
            0,
            0,
            1 << WARPEDMODEL_PREC_BITS,
        ];
        Self {
            gm_type: [IDENTITY; TOTAL_REFS_PER_FRAME],
            gm_params: [identity; TOTAL_REFS_PER_FRAME],
        }
    }
}

/// `film_grain_params()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct FilmGrain {
    pub apply_grain: bool,
    pub grain_seed: u16,
    pub update_grain: bool,
    pub film_grain_params_ref_idx: u8,
    pub num_y_points: u8,
    pub point_y_value: [u8; 14],
    pub point_y_scaling: [u8; 14],
    pub chroma_scaling_from_luma: bool,
    pub num_cb_points: u8,
    pub point_cb_value: [u8; 10],
    pub point_cb_scaling: [u8; 10],
    pub num_cr_points: u8,
    pub point_cr_value: [u8; 10],
    pub point_cr_scaling: [u8; 10],
    pub grain_scaling_minus_8: u8,
    pub ar_coeff_lag: u8,
    pub ar_coeffs_y_plus_128: [u8; 24],
    pub ar_coeffs_cb_plus_128: [u8; 25],
    pub ar_coeffs_cr_plus_128: [u8; 25],
    pub ar_coeff_shift_minus_6: u8,
    pub grain_scale_shift: u8,
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cb_offset: u16,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub cr_offset: u16,
    pub overlap_flag: bool,
    pub clip_to_restricted_range: bool,
}

/// A parsed frame header (`uncompressed_header()`), with the values its parsing derives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct FrameHeader {
    pub show_existing_frame: bool,
    pub frame_to_show_map_idx: u8,
    pub frame_type: FrameType,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub error_resilient_mode: bool,
    pub disable_cdf_update: bool,
    pub allow_screen_content_tools: bool,
    pub force_integer_mv: bool,
    pub current_frame_id: u32,
    pub frame_size_override_flag: bool,
    pub order_hint: u32,
    pub primary_ref_frame: u8,
    pub buffer_removal_time_present_flag: bool,
    pub refresh_frame_flags: u8,
    /// `ref_order_hint` (error resilient mode with order hints).
    pub ref_order_hint: [u32; NUM_REF_FRAMES],
    /// `FrameWidth` (after super-resolution's downscale), `FrameHeight`.
    pub frame_width: u32,
    pub frame_height: u32,
    /// `UpscaledWidth`.
    pub upscaled_width: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub render_and_frame_size_different: bool,
    pub use_superres: bool,
    pub coded_denom: u8,
    /// `SuperresDenom` (8 without super-resolution).
    pub superres_denom: u32,
    pub mi_cols: u32,
    pub mi_rows: u32,
    pub allow_intrabc: bool,
    pub frame_refs_short_signaling: bool,
    pub last_frame_idx: u8,
    pub gold_frame_idx: u8,
    /// `ref_frame_idx`: the slot of each of `LAST_FRAME` to `ALTREF_FRAME`.
    pub ref_frame_idx: [u8; REFS_PER_FRAME],
    pub delta_frame_id_minus_1: [u32; REFS_PER_FRAME],
    /// `expectedFrameId` per reference.
    pub expected_frame_id: [u32; REFS_PER_FRAME],
    pub allow_high_precision_mv: bool,
    pub is_filter_switchable: bool,
    /// `interpolation_filter` (4: switchable).
    pub interpolation_filter: u8,
    pub is_motion_mode_switchable: bool,
    pub use_ref_frame_mvs: bool,
    /// `OrderHints[refFrame]`, by reference frame (`[0]` unused).
    pub order_hints: [u32; TOTAL_REFS_PER_FRAME],
    /// `RefFrameSignBias`, by reference frame.
    pub ref_frame_sign_bias: [bool; TOTAL_REFS_PER_FRAME],
    pub disable_frame_end_update_cdf: bool,
    pub tile_info: TileInfo,
    pub quantization: Quantization,
    pub segmentation: Segmentation,
    pub deltas: Deltas,
    pub lossless_array: [bool; MAX_SEGMENTS],
    pub coded_lossless: bool,
    pub all_lossless: bool,
    pub loop_filter: LoopFilter,
    pub cdef: Cdef,
    pub restoration: Restoration,
    pub tx_mode_select: bool,
    /// `TxMode`: 0 only 4 × 4, 1 largest, 2 select.
    pub tx_mode: u8,
    pub reference_select: bool,
    pub skip_mode_present: bool,
    /// `SkipModeFrame`, when skip mode is allowed.
    pub skip_mode_frame: [u8; 2],
    pub allow_warped_motion: bool,
    pub reduced_tx_set: bool,
    pub global_motion: GlobalMotion,
    pub film_grain: FilmGrain,
    /// The OBU's temporal and spatial layer.
    pub temporal_id: u8,
    pub spatial_id: u8,
}

impl FrameHeader {
    /// `FrameIsIntra`.
    pub fn is_intra(&self) -> bool {
        self.frame_type.is_intra()
    }
}

/// What a reference slot holds (the reference frame update process's saved values).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct RefSlot {
    pub valid: bool,
    pub frame_id: u32,
    pub upscaled_width: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub mi_cols: u32,
    pub mi_rows: u32,
    pub frame_type: FrameType,
    pub showable_frame: bool,
    pub order_hint: u32,
    /// `SavedOrderHints[i][refFrame]`.
    pub saved_order_hints: [u32; TOTAL_REFS_PER_FRAME],
    pub gm_params: [[i32; 6]; TOTAL_REFS_PER_FRAME],
    pub loop_filter_ref_deltas: [i8; TOTAL_REFS_PER_FRAME],
    pub loop_filter_mode_deltas: [i8; 2],
    pub feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    pub feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
    pub film_grain: FilmGrain,
    pub bit_depth: u8,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
}

/// The reference frames' state between frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefState {
    /// The slots `ref_frame_idx` and `refresh_frame_flags` name.
    pub slots: [RefSlot; NUM_REF_FRAMES],
}

/// The loop filter deltas `setup_past_independence()` gives.
const DEFAULT_REF_DELTAS: [i8; TOTAL_REFS_PER_FRAME] = [1, 0, 0, 0, -1, 0, -1, -1];

const SEGMENTATION_FEATURE_BITS: [u32; SEG_LVL_MAX] = [8, 6, 6, 6, 6, 3, 0, 0];
const SEGMENTATION_FEATURE_SIGNED: [bool; SEG_LVL_MAX] =
    [true, true, true, true, true, false, false, false];
const SEGMENTATION_FEATURE_MAX: [i32; SEG_LVL_MAX] = [255, 63, 63, 63, 63, 7, 0, 0];

impl RefState {
    /// The reference frame update process (7.20) after `h` was decoded, and the frame
    /// loading process (7.21) when `h` shows an existing key frame.
    pub fn update(&mut self, h: &FrameHeader, seq: &SequenceHeader) {
        if h.show_existing_frame {
            if h.frame_type == FrameType::Key {
                // The loaded frame becomes the current one and refreshes every slot.
                let shown = self.slots[usize::from(h.frame_to_show_map_idx)];
                for slot in &mut self.slots {
                    *slot = shown;
                }
            }
            return;
        }
        let saved = RefSlot {
            valid: true,
            frame_id: h.current_frame_id,
            upscaled_width: h.upscaled_width,
            frame_width: h.frame_width,
            frame_height: h.frame_height,
            render_width: h.render_width,
            render_height: h.render_height,
            mi_cols: h.mi_cols,
            mi_rows: h.mi_rows,
            frame_type: h.frame_type,
            showable_frame: h.showable_frame,
            order_hint: h.order_hint,
            saved_order_hints: h.order_hints,
            gm_params: h.global_motion.gm_params,
            loop_filter_ref_deltas: h.loop_filter.ref_deltas,
            loop_filter_mode_deltas: h.loop_filter.mode_deltas,
            feature_enabled: h.segmentation.feature_enabled,
            feature_data: h.segmentation.feature_data,
            film_grain: h.film_grain,
            bit_depth: seq.color.bit_depth,
            subsampling_x: seq.color.subsampling_x,
            subsampling_y: seq.color.subsampling_y,
        };
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if h.refresh_frame_flags >> i & 1 != 0 {
                *slot = saved;
            }
        }
    }
}

/// `get_relative_dist(a, b)`.
pub fn relative_dist(seq: &SequenceHeader, a: u32, b: u32) -> i32 {
    if !seq.enable_order_hint {
        return 0;
    }
    let bits = seq.order_hint_bits;
    let diff = a as i32 - b as i32;
    let m = 1i32 << (bits - 1);
    (diff & (m - 1)) - (diff & m)
}

/// `tile_log2(blkSize, target)`.
fn tile_log2(blk_size: u32, target: u32) -> u32 {
    let mut k = 0;
    while (blk_size << k) < target {
        k += 1;
    }
    k
}

/// Parses a frame header OBU's payload (`frame_header_obu()` without the copies' check),
/// of an OBU in layer `temporal_id`, `spatial_id`, against the stream's sequence header
/// and reference state. Returns the header and the bits it took.
///
/// # Errors
///
/// A truncated or invalid header, or one naming a reference frame never decoded.
pub fn parse(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    refs: &RefState,
    temporal_id: u8,
    spatial_id: u8,
) -> Result<FrameHeader, Av1Error> {
    let mut h = FrameHeader {
        temporal_id,
        spatial_id,
        superres_denom: 8,
        tile_info: TileInfo::default(),
        global_motion: GlobalMotion::default(),
        ..FrameHeader::default()
    };
    let id_len = if seq.frame_id_numbers_present_flag {
        seq.frame_id_length()
    } else {
        0
    };
    let all_frames: u8 = 0xFF;
    let mut ref_valid = [false; NUM_REF_FRAMES];
    let mut ref_order_hint = [0u32; NUM_REF_FRAMES];
    for i in 0..NUM_REF_FRAMES {
        ref_valid[i] = refs.slots[i].valid;
        ref_order_hint[i] = refs.slots[i].order_hint;
    }
    let equal_picture_interval = seq.timing_info.is_some_and(|t| t.equal_picture_interval);
    if seq.reduced_still_picture_header {
        h.frame_type = FrameType::Key;
        h.show_frame = true;
    } else {
        h.show_existing_frame = r.flag()?;
        if h.show_existing_frame {
            h.frame_to_show_map_idx = r.f(3)? as u8;
            if let Some(model) = seq.decoder_model_info
                && !equal_picture_interval
            {
                // temporal_point_info(): frame_presentation_time.
                r.f(u32::from(model.frame_presentation_time_length_minus_1) + 1)?;
            }
            if seq.frame_id_numbers_present_flag {
                r.f(id_len)?;
            }
            let shown = refs.slots[usize::from(h.frame_to_show_map_idx)];
            if !shown.valid {
                return Err(Av1Error::MissingReference);
            }
            h.frame_type = shown.frame_type;
            h.refresh_frame_flags = if h.frame_type == FrameType::Key {
                all_frames
            } else {
                0
            };
            h.show_frame = true;
            h.frame_width = shown.frame_width;
            h.frame_height = shown.frame_height;
            h.upscaled_width = shown.upscaled_width;
            h.render_width = shown.render_width;
            h.render_height = shown.render_height;
            h.mi_cols = shown.mi_cols;
            h.mi_rows = shown.mi_rows;
            h.order_hint = shown.order_hint;
            h.current_frame_id = shown.frame_id;
            if seq.film_grain_params_present {
                // load_grain_params(frame_to_show_map_idx).
                h.film_grain = shown.film_grain;
            }
            return Ok(h);
        }
        h.frame_type = FrameType::from_bits(r.f(2)?);
        h.show_frame = r.flag()?;
        if h.show_frame
            && let Some(model) = seq.decoder_model_info
            && !equal_picture_interval
        {
            r.f(u32::from(model.frame_presentation_time_length_minus_1) + 1)?;
        }
        h.showable_frame = if h.show_frame {
            h.frame_type != FrameType::Key
        } else {
            r.flag()?
        };
        h.error_resilient_mode = if h.frame_type == FrameType::Switch
            || (h.frame_type == FrameType::Key && h.show_frame)
        {
            true
        } else {
            r.flag()?
        };
    }
    if h.frame_type == FrameType::Key && h.show_frame {
        ref_valid = [false; NUM_REF_FRAMES];
        ref_order_hint = [0; NUM_REF_FRAMES];
        h.order_hints = [0; TOTAL_REFS_PER_FRAME];
    }
    h.disable_cdf_update = r.flag()?;
    h.allow_screen_content_tools = if seq.seq_force_screen_content_tools == SELECT {
        r.flag()?
    } else {
        seq.seq_force_screen_content_tools != 0
    };
    h.force_integer_mv = if h.allow_screen_content_tools {
        if seq.seq_force_integer_mv == SELECT {
            r.flag()?
        } else {
            seq.seq_force_integer_mv != 0
        }
    } else {
        false
    };
    if h.is_intra() {
        h.force_integer_mv = true;
    }
    if seq.frame_id_numbers_present_flag {
        h.current_frame_id = r.f(id_len)?;
        // mark_ref_frames(idLen).
        let diff_len = u32::from(seq.delta_frame_id_length_minus_2) + 2;
        let current = h.current_frame_id;
        for (valid, slot) in ref_valid.iter_mut().zip(&refs.slots) {
            let id = slot.frame_id;
            if current > (1 << diff_len) {
                if id > current || id < current - (1 << diff_len) {
                    *valid = false;
                }
            } else if id > current && id < (1 << id_len) + current - (1 << diff_len) {
                *valid = false;
            }
        }
    }
    h.frame_size_override_flag = if h.frame_type == FrameType::Switch {
        true
    } else if seq.reduced_still_picture_header {
        false
    } else {
        r.flag()?
    };
    h.order_hint = r.f(seq.order_hint_bits)?;
    h.primary_ref_frame = if h.is_intra() || h.error_resilient_mode {
        PRIMARY_REF_NONE
    } else {
        r.f(3)? as u8
    };
    if let Some(model) = seq.decoder_model_info {
        h.buffer_removal_time_present_flag = r.flag()?;
        if h.buffer_removal_time_present_flag {
            for op in &seq.operating_points[..seq.operating_points_count] {
                if op.decoder_model_present {
                    let idc = op.idc;
                    let in_temporal = (idc >> temporal_id) & 1 != 0;
                    let in_spatial = (idc >> (u32::from(spatial_id) + 8)) & 1 != 0;
                    if idc == 0 || (in_temporal && in_spatial) {
                        r.f(u32::from(model.buffer_removal_time_length_minus_1) + 1)?;
                    }
                }
            }
        }
    }
    h.refresh_frame_flags =
        if h.frame_type == FrameType::Switch || (h.frame_type == FrameType::Key && h.show_frame) {
            all_frames
        } else {
            r.f(8)? as u8
        };
    if (!h.is_intra() || h.refresh_frame_flags != all_frames)
        && h.error_resilient_mode
        && seq.enable_order_hint
    {
        for i in 0..NUM_REF_FRAMES {
            h.ref_order_hint[i] = r.f(seq.order_hint_bits)?;
            if h.ref_order_hint[i] != ref_order_hint[i] {
                ref_valid[i] = false;
                ref_order_hint[i] = h.ref_order_hint[i];
            }
        }
    }
    if h.is_intra() {
        frame_size(r, seq, &mut h)?;
        render_size(r, &mut h)?;
        if h.allow_screen_content_tools && h.upscaled_width == h.frame_width {
            h.allow_intrabc = r.flag()?;
        }
    } else {
        if seq.enable_order_hint {
            h.frame_refs_short_signaling = r.flag()?;
            if h.frame_refs_short_signaling {
                h.last_frame_idx = r.f(3)? as u8;
                h.gold_frame_idx = r.f(3)? as u8;
                set_frame_refs(seq, &mut h, &ref_order_hint);
            }
        }
        for i in 0..REFS_PER_FRAME {
            if !h.frame_refs_short_signaling {
                h.ref_frame_idx[i] = r.f(3)? as u8;
            }
            if seq.frame_id_numbers_present_flag {
                h.delta_frame_id_minus_1[i] =
                    r.f(u32::from(seq.delta_frame_id_length_minus_2) + 2)?;
                let delta = h.delta_frame_id_minus_1[i] + 1;
                h.expected_frame_id[i] =
                    (h.current_frame_id + (1 << id_len) - delta) % (1 << id_len);
            }
        }
        for &idx in &h.ref_frame_idx {
            if !ref_valid[usize::from(idx)] && !h.error_resilient_mode {
                return Err(Av1Error::MissingReference);
            }
        }
        if h.frame_size_override_flag && !h.error_resilient_mode {
            frame_size_with_refs(r, seq, refs, &mut h)?;
        } else {
            frame_size(r, seq, &mut h)?;
            render_size(r, &mut h)?;
        }
        h.allow_high_precision_mv = if h.force_integer_mv { false } else { r.flag()? };
        // read_interpolation_filter().
        h.is_filter_switchable = r.flag()?;
        h.interpolation_filter = if h.is_filter_switchable {
            4
        } else {
            r.f(2)? as u8
        };
        h.is_motion_mode_switchable = r.flag()?;
        h.use_ref_frame_mvs = if h.error_resilient_mode || !seq.enable_ref_frame_mvs {
            false
        } else {
            r.flag()?
        };
        for i in 0..REFS_PER_FRAME {
            let ref_frame = LAST_FRAME + i;
            let hint = ref_order_hint[usize::from(h.ref_frame_idx[i])];
            h.order_hints[ref_frame] = hint;
            h.ref_frame_sign_bias[ref_frame] =
                seq.enable_order_hint && relative_dist(seq, hint, h.order_hint) > 0;
        }
    }
    h.disable_frame_end_update_cdf = if seq.reduced_still_picture_header || h.disable_cdf_update {
        true
    } else {
        r.flag()?
    };
    // setup_past_independence() or load_previous(): the loop filter deltas, the segmentation
    // features and the previous global motion parameters the header updates.
    let mut prev_gm = GlobalMotion::default().gm_params;
    if h.primary_ref_frame == PRIMARY_REF_NONE {
        h.loop_filter.ref_deltas = DEFAULT_REF_DELTAS;
        h.loop_filter.mode_deltas = [0, 0];
    } else {
        let prev = &refs.slots[usize::from(h.ref_frame_idx[usize::from(h.primary_ref_frame)])];
        prev_gm = prev.gm_params;
        h.loop_filter.ref_deltas = prev.loop_filter_ref_deltas;
        h.loop_filter.mode_deltas = prev.loop_filter_mode_deltas;
        h.segmentation.feature_enabled = prev.feature_enabled;
        h.segmentation.feature_data = prev.feature_data;
    }
    tile_info(r, seq, &mut h)?;
    quantization_params(r, seq, &mut h.quantization)?;
    segmentation_params(r, &mut h)?;
    // delta_q_params(), delta_lf_params().
    if h.quantization.base_q_idx > 0 {
        h.deltas.delta_q_present = r.flag()?;
    }
    if h.deltas.delta_q_present {
        h.deltas.delta_q_res = r.f(2)? as u8;
        if !h.allow_intrabc {
            h.deltas.delta_lf_present = r.flag()?;
        }
        if h.deltas.delta_lf_present {
            h.deltas.delta_lf_res = r.f(2)? as u8;
            h.deltas.delta_lf_multi = r.flag()?;
        }
    }
    h.coded_lossless = true;
    let q = h.quantization;
    for segment in 0..MAX_SEGMENTS {
        let qindex = qindex(&h, segment);
        let lossless = qindex == 0
            && q.delta_q_y_dc == 0
            && q.delta_q_u_ac == 0
            && q.delta_q_u_dc == 0
            && q.delta_q_v_ac == 0
            && q.delta_q_v_dc == 0;
        h.lossless_array[segment] = lossless;
        if !lossless {
            h.coded_lossless = false;
        }
    }
    h.all_lossless = h.coded_lossless && h.frame_width == h.upscaled_width;
    loop_filter_params(r, seq, &mut h)?;
    cdef_params(r, seq, &mut h)?;
    lr_params(r, seq, &mut h)?;
    // read_tx_mode().
    if h.coded_lossless {
        h.tx_mode = 0;
    } else {
        h.tx_mode_select = r.flag()?;
        h.tx_mode = if h.tx_mode_select { 2 } else { 1 };
    }
    // frame_reference_mode().
    h.reference_select = if h.is_intra() { false } else { r.flag()? };
    skip_mode_params(r, seq, &mut h, &ref_order_hint)?;
    h.allow_warped_motion = if h.is_intra() || h.error_resilient_mode || !seq.enable_warped_motion {
        false
    } else {
        r.flag()?
    };
    h.reduced_tx_set = r.flag()?;
    global_motion_params(r, &mut h, &prev_gm)?;
    film_grain_params(r, seq, refs, &mut h)?;
    Ok(h)
}

/// `get_qindex(1, segmentId)`.
fn qindex(h: &FrameHeader, segment: usize) -> i32 {
    let base = i32::from(h.quantization.base_q_idx);
    if h.segmentation.enabled && h.segmentation.feature_enabled[segment][0] {
        let data = i32::from(h.segmentation.feature_data[segment][0]);
        return (base + data).clamp(0, 255);
    }
    base
}

fn frame_size(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    if h.frame_size_override_flag {
        h.frame_width = r.f(u32::from(seq.frame_width_bits_minus_1) + 1)? + 1;
        h.frame_height = r.f(u32::from(seq.frame_height_bits_minus_1) + 1)? + 1;
    } else {
        h.frame_width = seq.max_width();
        h.frame_height = seq.max_height();
    }
    superres_params(r, seq, h)?;
    compute_image_size(h);
    Ok(())
}

fn superres_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    h.use_superres = if seq.enable_superres {
        r.flag()?
    } else {
        false
    };
    if h.use_superres {
        h.coded_denom = r.f(3)? as u8;
        h.superres_denom = u32::from(h.coded_denom) + 9;
    } else {
        h.superres_denom = 8;
    }
    h.upscaled_width = h.frame_width;
    h.frame_width = (h.upscaled_width * 8 + h.superres_denom / 2) / h.superres_denom;
    Ok(())
}

fn compute_image_size(h: &mut FrameHeader) {
    h.mi_cols = 2 * ((h.frame_width + 7) >> 3);
    h.mi_rows = 2 * ((h.frame_height + 7) >> 3);
}

fn render_size(r: &mut BitReader<'_>, h: &mut FrameHeader) -> Result<(), Av1Error> {
    h.render_and_frame_size_different = r.flag()?;
    if h.render_and_frame_size_different {
        h.render_width = r.f(16)? + 1;
        h.render_height = r.f(16)? + 1;
    } else {
        h.render_width = h.upscaled_width;
        h.render_height = h.frame_height;
    }
    Ok(())
}

fn frame_size_with_refs(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    refs: &RefState,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    for i in 0..REFS_PER_FRAME {
        if r.flag()? {
            let slot = &refs.slots[usize::from(h.ref_frame_idx[i])];
            h.upscaled_width = slot.upscaled_width;
            h.frame_width = h.upscaled_width;
            h.frame_height = slot.frame_height;
            h.render_width = slot.render_width;
            h.render_height = slot.render_height;
            superres_params(r, seq, h)?;
            compute_image_size(h);
            return Ok(());
        }
    }
    frame_size(r, seq, h)?;
    render_size(r, h)
}

/// `set_frame_refs()` (7.8): the references of a frame that signals only its last and
/// golden frames.
fn set_frame_refs(
    seq: &SequenceHeader,
    h: &mut FrameHeader,
    ref_order_hint: &[u32; NUM_REF_FRAMES],
) {
    let mut idx = [-1i32; REFS_PER_FRAME];
    // LAST_FRAME is the first reference.
    idx[0] = i32::from(h.last_frame_idx);
    idx[GOLDEN_FRAME - LAST_FRAME] = i32::from(h.gold_frame_idx);
    let mut used = [false; NUM_REF_FRAMES];
    used[usize::from(h.last_frame_idx)] = true;
    used[usize::from(h.gold_frame_idx)] = true;
    let cur = 1i32 << (seq.order_hint_bits - 1);
    let mut shifted = [0i32; NUM_REF_FRAMES];
    for i in 0..NUM_REF_FRAMES {
        shifted[i] = cur + relative_dist(seq, ref_order_hint[i], h.order_hint);
    }
    let find = |used: &[bool; NUM_REF_FRAMES], backward: bool, latest: bool| -> i32 {
        let mut found = -1i32;
        let mut best = 0;
        for i in 0..NUM_REF_FRAMES {
            let hint = shifted[i];
            let side = if backward { hint >= cur } else { hint < cur };
            if !used[i] && side {
                let better = if latest { hint >= best } else { hint < best };
                if found < 0 || better {
                    found = i as i32;
                    best = hint;
                }
            }
        }
        found
    };
    // ALTREF: the latest backward reference; BWDREF and ALTREF2: the earliest ones.
    for (frame, latest) in [
        (ALTREF_FRAME, true),
        (BWDREF_FRAME, false),
        (ALTREF2_FRAME, false),
    ] {
        let r = find(&used, true, latest);
        if r >= 0 {
            idx[frame - LAST_FRAME] = r;
            used[r as usize] = true;
        }
    }
    // The rest from the latest forward references.
    for frame in [
        LAST2_FRAME,
        LAST3_FRAME,
        BWDREF_FRAME,
        ALTREF2_FRAME,
        ALTREF_FRAME,
    ] {
        if idx[frame - LAST_FRAME] < 0 {
            let r = find(&used, false, true);
            if r >= 0 {
                idx[frame - LAST_FRAME] = r;
                used[r as usize] = true;
            }
        }
    }
    // Anything still unset: the earliest reference of all.
    let mut earliest = -1i32;
    let mut earliest_hint = 0;
    for (i, &hint) in shifted.iter().enumerate() {
        if earliest < 0 || hint < earliest_hint {
            earliest = i as i32;
            earliest_hint = hint;
        }
    }
    for (out, &i) in h.ref_frame_idx.iter_mut().zip(&idx) {
        *out = if i < 0 { earliest as u8 } else { i as u8 };
    }
}

fn tile_info(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    let t = &mut h.tile_info;
    let (sb_cols, sb_rows, sb_shift) = if seq.use_128x128_superblock {
        ((h.mi_cols + 31) >> 5, (h.mi_rows + 31) >> 5, 5)
    } else {
        ((h.mi_cols + 15) >> 4, (h.mi_rows + 15) >> 4, 4)
    };
    let sb_size = sb_shift + 2;
    let max_tile_width_sb = 4096 >> sb_size;
    let mut max_tile_area_sb = (4096 * 2304) >> (2 * sb_size);
    let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
    let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS as u32));
    let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS as u32));
    let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
    t.uniform_tile_spacing_flag = r.flag()?;
    if t.uniform_tile_spacing_flag {
        t.tile_cols_log2 = min_log2_tile_cols;
        while t.tile_cols_log2 < max_log2_tile_cols {
            if r.flag()? {
                t.tile_cols_log2 += 1;
            } else {
                break;
            }
        }
        let tile_width_sb = (sb_cols + (1 << t.tile_cols_log2) - 1) >> t.tile_cols_log2;
        let mut i = 0;
        let mut start = 0;
        while start < sb_cols {
            if i >= MAX_TILE_COLS {
                return Err(Av1Error::Corrupt("more tile columns than allowed"));
            }
            t.mi_col_starts[i] = (start << sb_shift) as u16;
            t.width_in_sbs_minus_1[i] = (tile_width_sb.min(sb_cols - start) - 1) as u16;
            i += 1;
            start += tile_width_sb;
        }
        t.mi_col_starts[i] = h.mi_cols as u16;
        t.tile_cols = i as u32;
        let min_log2_tile_rows = min_log2_tiles.saturating_sub(t.tile_cols_log2);
        t.tile_rows_log2 = min_log2_tile_rows;
        while t.tile_rows_log2 < max_log2_tile_rows {
            if r.flag()? {
                t.tile_rows_log2 += 1;
            } else {
                break;
            }
        }
        let tile_height_sb = (sb_rows + (1 << t.tile_rows_log2) - 1) >> t.tile_rows_log2;
        let mut i = 0;
        let mut start = 0;
        while start < sb_rows {
            if i >= MAX_TILE_ROWS {
                return Err(Av1Error::Corrupt("more tile rows than allowed"));
            }
            t.mi_row_starts[i] = (start << sb_shift) as u16;
            t.height_in_sbs_minus_1[i] = (tile_height_sb.min(sb_rows - start) - 1) as u16;
            i += 1;
            start += tile_height_sb;
        }
        t.mi_row_starts[i] = h.mi_rows as u16;
        t.tile_rows = i as u32;
    } else {
        let mut widest = 0;
        let mut start = 0;
        let mut i = 0;
        while start < sb_cols {
            if i >= MAX_TILE_COLS {
                return Err(Av1Error::Corrupt("more tile columns than allowed"));
            }
            t.mi_col_starts[i] = (start << sb_shift) as u16;
            let max_width = (sb_cols - start).min(max_tile_width_sb);
            let size = r.ns(max_width)? + 1;
            t.width_in_sbs_minus_1[i] = (size - 1) as u16;
            widest = widest.max(size);
            start += size;
            i += 1;
        }
        t.mi_col_starts[i] = h.mi_cols as u16;
        t.tile_cols = i as u32;
        t.tile_cols_log2 = tile_log2(1, t.tile_cols);
        max_tile_area_sb = if min_log2_tiles > 0 {
            (sb_rows * sb_cols) >> (min_log2_tiles + 1)
        } else {
            sb_rows * sb_cols
        };
        let max_tile_height_sb = (max_tile_area_sb / widest).max(1);
        let mut start = 0;
        let mut i = 0;
        while start < sb_rows {
            if i >= MAX_TILE_ROWS {
                return Err(Av1Error::Corrupt("more tile rows than allowed"));
            }
            t.mi_row_starts[i] = (start << sb_shift) as u16;
            let max_height = (sb_rows - start).min(max_tile_height_sb);
            let size = r.ns(max_height)? + 1;
            t.height_in_sbs_minus_1[i] = (size - 1) as u16;
            start += size;
            i += 1;
        }
        t.mi_row_starts[i] = h.mi_rows as u16;
        t.tile_rows = i as u32;
        t.tile_rows_log2 = tile_log2(1, t.tile_rows);
    }
    if t.tile_cols_log2 > 0 || t.tile_rows_log2 > 0 {
        t.context_update_tile_id = r.f(t.tile_rows_log2 + t.tile_cols_log2)?;
        t.tile_size_bytes_minus_1 = r.f(2)? as u8;
    } else {
        t.context_update_tile_id = 0;
    }
    Ok(())
}

fn read_delta_q(r: &mut BitReader<'_>) -> Result<i8, Av1Error> {
    Ok(if r.flag()? { r.su(7)? as i8 } else { 0 })
}

fn quantization_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    q: &mut Quantization,
) -> Result<(), Av1Error> {
    q.base_q_idx = r.f(8)? as u8;
    q.delta_q_y_dc = read_delta_q(r)?;
    if seq.color.num_planes() > 1 {
        q.diff_uv_delta = if seq.color.separate_uv_delta_q {
            r.flag()?
        } else {
            false
        };
        q.delta_q_u_dc = read_delta_q(r)?;
        q.delta_q_u_ac = read_delta_q(r)?;
        if q.diff_uv_delta {
            q.delta_q_v_dc = read_delta_q(r)?;
            q.delta_q_v_ac = read_delta_q(r)?;
        } else {
            q.delta_q_v_dc = q.delta_q_u_dc;
            q.delta_q_v_ac = q.delta_q_u_ac;
        }
    }
    q.using_qmatrix = r.flag()?;
    if q.using_qmatrix {
        q.qm_y = r.f(4)? as u8;
        q.qm_u = r.f(4)? as u8;
        q.qm_v = if seq.color.separate_uv_delta_q {
            r.f(4)? as u8
        } else {
            q.qm_u
        };
    }
    Ok(())
}

fn segmentation_params(r: &mut BitReader<'_>, h: &mut FrameHeader) -> Result<(), Av1Error> {
    let s = &mut h.segmentation;
    s.enabled = r.flag()?;
    if s.enabled {
        if h.primary_ref_frame == PRIMARY_REF_NONE {
            s.update_map = true;
            s.temporal_update = false;
            s.update_data = true;
        } else {
            s.update_map = r.flag()?;
            if s.update_map {
                s.temporal_update = r.flag()?;
            }
            s.update_data = r.flag()?;
        }
        if s.update_data {
            for i in 0..MAX_SEGMENTS {
                for j in 0..SEG_LVL_MAX {
                    let enabled = r.flag()?;
                    s.feature_enabled[i][j] = enabled;
                    let mut value = 0;
                    if enabled {
                        let bits = SEGMENTATION_FEATURE_BITS[j];
                        let limit = SEGMENTATION_FEATURE_MAX[j];
                        value = if SEGMENTATION_FEATURE_SIGNED[j] {
                            r.su(1 + bits)?.clamp(-limit, limit)
                        } else {
                            (r.f(bits)? as i32).clamp(0, limit)
                        };
                    }
                    s.feature_data[i][j] = value as i16;
                }
            }
        }
    } else {
        s.feature_enabled = [[false; SEG_LVL_MAX]; MAX_SEGMENTS];
        s.feature_data = [[0; SEG_LVL_MAX]; MAX_SEGMENTS];
    }
    s.seg_id_pre_skip = false;
    s.last_active_seg_id = 0;
    for i in 0..MAX_SEGMENTS {
        for j in 0..SEG_LVL_MAX {
            if s.feature_enabled[i][j] {
                s.last_active_seg_id = i as u8;
                if j >= 5 {
                    s.seg_id_pre_skip = true;
                }
            }
        }
    }
    Ok(())
}

fn loop_filter_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    let lf = &mut h.loop_filter;
    if h.coded_lossless || h.allow_intrabc {
        // Not coded: the filter is off. The deltas' flags as setup_past_independence()
        // leaves them (and rav1d reports them).
        lf.level = [0; 4];
        lf.delta_enabled = true;
        lf.delta_update = true;
        lf.ref_deltas = DEFAULT_REF_DELTAS;
        lf.mode_deltas = [0, 0];
        return Ok(());
    }
    lf.level[0] = r.f(6)? as u8;
    lf.level[1] = r.f(6)? as u8;
    if seq.color.num_planes() > 1 && (lf.level[0] != 0 || lf.level[1] != 0) {
        lf.level[2] = r.f(6)? as u8;
        lf.level[3] = r.f(6)? as u8;
    }
    lf.sharpness = r.f(3)? as u8;
    lf.delta_enabled = r.flag()?;
    if lf.delta_enabled {
        lf.delta_update = r.flag()?;
        if lf.delta_update {
            for i in 0..TOTAL_REFS_PER_FRAME {
                if r.flag()? {
                    lf.update_ref_delta |= 1 << i;
                    lf.ref_deltas[i] = r.su(7)? as i8;
                }
            }
            for i in 0..2 {
                if r.flag()? {
                    lf.update_mode_delta |= 1 << i;
                    lf.mode_deltas[i] = r.su(7)? as i8;
                }
            }
        }
    }
    Ok(())
}

fn cdef_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    let c = &mut h.cdef;
    if h.coded_lossless || h.allow_intrabc || !seq.enable_cdef {
        *c = Cdef::default();
        return Ok(());
    }
    c.damping_minus_3 = r.f(2)? as u8;
    c.bits = r.f(2)? as u8;
    for i in 0..(1usize << c.bits) {
        c.y_pri_strength[i] = r.f(4)? as u8;
        c.y_sec_strength[i] = r.f(2)? as u8;
        if seq.color.num_planes() > 1 {
            c.uv_pri_strength[i] = r.f(4)? as u8;
            c.uv_sec_strength[i] = r.f(2)? as u8;
        }
    }
    Ok(())
}

fn lr_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    let lr = &mut h.restoration;
    if h.all_lossless || h.allow_intrabc || !seq.enable_restoration {
        *lr = Restoration::default();
        return Ok(());
    }
    // Remap_Lr_Type.
    const REMAP: [u8; 4] = [0, 3, 1, 2];
    for plane in 0..seq.color.num_planes() as usize {
        let t = REMAP[r.f(2)? as usize];
        lr.frame_restoration_type[plane] = t;
        if t != 0 {
            lr.uses_lr = true;
            if plane > 0 {
                lr.uses_chroma_lr = true;
            }
        }
    }
    if lr.uses_lr {
        if seq.use_128x128_superblock {
            lr.lr_unit_shift = r.f(1)? as u8 + 1;
        } else {
            lr.lr_unit_shift = r.f(1)? as u8;
            if lr.lr_unit_shift != 0 {
                lr.lr_unit_shift += r.f(1)? as u8;
            }
        }
        let size = 256u16 >> (2 - lr.lr_unit_shift);
        lr.loop_restoration_size[0] = size;
        lr.lr_uv_shift = if seq.color.subsampling_x && seq.color.subsampling_y && lr.uses_chroma_lr
        {
            r.f(1)? as u8
        } else {
            0
        };
        lr.loop_restoration_size[1] = size >> lr.lr_uv_shift;
        lr.loop_restoration_size[2] = size >> lr.lr_uv_shift;
    }
    Ok(())
}

fn skip_mode_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    h: &mut FrameHeader,
    ref_order_hint: &[u32; NUM_REF_FRAMES],
) -> Result<(), Av1Error> {
    let mut allowed = false;
    if !h.is_intra() && h.reference_select && seq.enable_order_hint {
        let (mut forward, mut forward_hint) = (-1i32, 0u32);
        let (mut backward, mut backward_hint) = (-1i32, 0u32);
        for i in 0..REFS_PER_FRAME {
            let hint = ref_order_hint[usize::from(h.ref_frame_idx[i])];
            if relative_dist(seq, hint, h.order_hint) < 0 {
                if forward < 0 || relative_dist(seq, hint, forward_hint) > 0 {
                    forward = i as i32;
                    forward_hint = hint;
                }
            } else if relative_dist(seq, hint, h.order_hint) > 0
                && (backward < 0 || relative_dist(seq, hint, backward_hint) < 0)
            {
                backward = i as i32;
                backward_hint = hint;
            }
        }
        if forward >= 0 {
            if backward >= 0 {
                allowed = true;
                h.skip_mode_frame = [
                    (LAST_FRAME as i32 + forward.min(backward)) as u8,
                    (LAST_FRAME as i32 + forward.max(backward)) as u8,
                ];
            } else {
                let (mut second, mut second_hint) = (-1i32, 0u32);
                for i in 0..REFS_PER_FRAME {
                    let hint = ref_order_hint[usize::from(h.ref_frame_idx[i])];
                    if relative_dist(seq, hint, forward_hint) < 0
                        && (second < 0 || relative_dist(seq, hint, second_hint) > 0)
                    {
                        second = i as i32;
                        second_hint = hint;
                    }
                }
                if second >= 0 {
                    allowed = true;
                    h.skip_mode_frame = [
                        (LAST_FRAME as i32 + forward.min(second)) as u8,
                        (LAST_FRAME as i32 + forward.max(second)) as u8,
                    ];
                }
            }
        }
    }
    h.skip_mode_present = if allowed { r.flag()? } else { false };
    Ok(())
}

fn global_motion_params(
    r: &mut BitReader<'_>,
    h: &mut FrameHeader,
    prev: &[[i32; 6]; TOTAL_REFS_PER_FRAME],
) -> Result<(), Av1Error> {
    h.global_motion = GlobalMotion::default();
    if h.is_intra() {
        return Ok(());
    }
    for reference in LAST_FRAME..=ALTREF_FRAME {
        let kind = if r.flag()? {
            if r.flag()? {
                ROTZOOM
            } else if r.flag()? {
                TRANSLATION
            } else {
                AFFINE
            }
        } else {
            IDENTITY
        };
        h.global_motion.gm_type[reference] = kind;
        if kind >= ROTZOOM {
            read_global_param(r, h, prev, kind, reference, 2)?;
            read_global_param(r, h, prev, kind, reference, 3)?;
            if kind == AFFINE {
                read_global_param(r, h, prev, kind, reference, 4)?;
                read_global_param(r, h, prev, kind, reference, 5)?;
            } else {
                let p = &mut h.global_motion.gm_params[reference];
                p[4] = -p[3];
                p[5] = p[2];
            }
        }
        if kind >= TRANSLATION {
            read_global_param(r, h, prev, kind, reference, 0)?;
            read_global_param(r, h, prev, kind, reference, 1)?;
        }
    }
    Ok(())
}

fn read_global_param(
    r: &mut BitReader<'_>,
    h: &mut FrameHeader,
    prev: &[[i32; 6]; TOTAL_REFS_PER_FRAME],
    kind: u8,
    reference: usize,
    idx: usize,
) -> Result<(), Av1Error> {
    let hp = u32::from(!h.allow_high_precision_mv);
    let (abs_bits, prec_bits) = if idx < 2 {
        if kind == TRANSLATION {
            (9 - hp, 3 - hp)
        } else {
            (12, 6)
        }
    } else {
        (12, 15)
    };
    let prec_diff = WARPEDMODEL_PREC_BITS - prec_bits;
    let round = if idx % 3 == 2 {
        1i32 << WARPEDMODEL_PREC_BITS
    } else {
        0
    };
    let sub = if idx % 3 == 2 { 1i32 << prec_bits } else { 0 };
    let mx = 1i32 << abs_bits;
    let reference_value = (prev[reference][idx] >> prec_diff) - sub;
    let value = decode_signed_subexp_with_ref(r, -mx, mx + 1, reference_value)?;
    h.global_motion.gm_params[reference][idx] = (value << prec_diff).wrapping_add(round);
    Ok(())
}

fn decode_signed_subexp_with_ref(
    r: &mut BitReader<'_>,
    low: i32,
    high: i32,
    reference: i32,
) -> Result<i32, Av1Error> {
    let x = decode_unsigned_subexp_with_ref(
        r,
        (high - low) as u32,
        reference.wrapping_sub(low) as u32,
    )?;
    Ok((x as i32).wrapping_add(low))
}

fn decode_unsigned_subexp_with_ref(
    r: &mut BitReader<'_>,
    mx: u32,
    reference: u32,
) -> Result<u32, Av1Error> {
    let v = decode_subexp(r, mx)?;
    // A damaged stream may give a reference outside 0..mx: wrap rather than panic.
    Ok(if reference <= mx / 2 {
        inverse_recenter(reference, v)
    } else {
        (mx - 1).wrapping_sub(inverse_recenter((mx - 1).wrapping_sub(reference), v))
    })
}

fn decode_subexp(r: &mut BitReader<'_>, num_syms: u32) -> Result<u32, Av1Error> {
    let (mut i, mut mk, k) = (0u32, 0u32, 3u32);
    loop {
        let b2 = if i > 0 { k + i - 1 } else { k };
        let a = 1u32 << b2;
        if num_syms <= mk + 3 * a {
            return Ok(r.ns(num_syms - mk)? + mk);
        }
        if r.flag()? {
            i += 1;
            mk += a;
        } else {
            return Ok(r.f(b2)? + mk);
        }
    }
}

fn inverse_recenter(r: u32, v: u32) -> u32 {
    if u64::from(v) > 2 * u64::from(r) {
        v
    } else if v & 1 != 0 {
        r - ((v + 1) >> 1)
    } else {
        r.wrapping_add(v >> 1)
    }
}

fn film_grain_params(
    r: &mut BitReader<'_>,
    seq: &SequenceHeader,
    refs: &RefState,
    h: &mut FrameHeader,
) -> Result<(), Av1Error> {
    if !seq.film_grain_params_present || (!h.show_frame && !h.showable_frame) {
        h.film_grain = FilmGrain::default();
        return Ok(());
    }
    let mut g = FilmGrain {
        apply_grain: r.flag()?,
        ..FilmGrain::default()
    };
    if !g.apply_grain {
        h.film_grain = FilmGrain::default();
        return Ok(());
    }
    g.grain_seed = r.f(16)? as u16;
    g.update_grain = if h.frame_type == FrameType::Inter {
        r.flag()?
    } else {
        true
    };
    if !g.update_grain {
        g.film_grain_params_ref_idx = r.f(3)? as u8;
        let seed = g.grain_seed;
        // load_grain_params(film_grain_params_ref_idx), keeping this frame's seed.
        let loaded = refs.slots[usize::from(g.film_grain_params_ref_idx)].film_grain;
        let reference = g.film_grain_params_ref_idx;
        g = loaded;
        g.grain_seed = seed;
        g.update_grain = false;
        g.film_grain_params_ref_idx = reference;
        h.film_grain = g;
        return Ok(());
    }
    g.num_y_points = r.f(4)? as u8;
    if g.num_y_points > 14 {
        return Err(Av1Error::Corrupt("more than 14 luma grain points"));
    }
    for i in 0..usize::from(g.num_y_points) {
        g.point_y_value[i] = r.f(8)? as u8;
        g.point_y_scaling[i] = r.f(8)? as u8;
    }
    g.chroma_scaling_from_luma = if seq.color.mono_chrome {
        false
    } else {
        r.flag()?
    };
    if seq.color.mono_chrome
        || g.chroma_scaling_from_luma
        || (seq.color.subsampling_x && seq.color.subsampling_y && g.num_y_points == 0)
    {
        g.num_cb_points = 0;
        g.num_cr_points = 0;
    } else {
        g.num_cb_points = r.f(4)? as u8;
        if g.num_cb_points > 10 {
            return Err(Av1Error::Corrupt("more than 10 chroma grain points"));
        }
        for i in 0..usize::from(g.num_cb_points) {
            g.point_cb_value[i] = r.f(8)? as u8;
            g.point_cb_scaling[i] = r.f(8)? as u8;
        }
        g.num_cr_points = r.f(4)? as u8;
        if g.num_cr_points > 10 {
            return Err(Av1Error::Corrupt("more than 10 chroma grain points"));
        }
        for i in 0..usize::from(g.num_cr_points) {
            g.point_cr_value[i] = r.f(8)? as u8;
            g.point_cr_scaling[i] = r.f(8)? as u8;
        }
    }
    g.grain_scaling_minus_8 = r.f(2)? as u8;
    g.ar_coeff_lag = r.f(2)? as u8;
    let lag = usize::from(g.ar_coeff_lag);
    let num_pos_luma = 2 * lag * (lag + 1);
    let num_pos_chroma = if g.num_y_points > 0 {
        for i in 0..num_pos_luma {
            g.ar_coeffs_y_plus_128[i] = r.f(8)? as u8;
        }
        num_pos_luma + 1
    } else {
        num_pos_luma
    };
    if g.chroma_scaling_from_luma || g.num_cb_points > 0 {
        for i in 0..num_pos_chroma {
            g.ar_coeffs_cb_plus_128[i] = r.f(8)? as u8;
        }
    }
    if g.chroma_scaling_from_luma || g.num_cr_points > 0 {
        for i in 0..num_pos_chroma {
            g.ar_coeffs_cr_plus_128[i] = r.f(8)? as u8;
        }
    }
    g.ar_coeff_shift_minus_6 = r.f(2)? as u8;
    g.grain_scale_shift = r.f(2)? as u8;
    if g.num_cb_points > 0 {
        g.cb_mult = r.f(8)? as u8;
        g.cb_luma_mult = r.f(8)? as u8;
        g.cb_offset = r.f(9)? as u16;
    }
    if g.num_cr_points > 0 {
        g.cr_mult = r.f(8)? as u8;
        g.cr_luma_mult = r.f(8)? as u8;
        g.cr_offset = r.f(9)? as u16;
    }
    g.overlap_flag = r.flag()?;
    g.clip_to_restricted_range = r.flag()?;
    h.film_grain = g;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence() -> SequenceHeader {
        SequenceHeader {
            enable_order_hint: true,
            order_hint_bits_minus_1: 6,
            order_hint_bits: 7,
            ..SequenceHeader::default()
        }
    }

    #[test]
    fn relative_distances_wrap() {
        let s = sequence();
        assert_eq!(relative_dist(&s, 1, 127), 2);
        assert_eq!(relative_dist(&s, 127, 1), -2);
        assert_eq!(relative_dist(&s, 10, 4), 6);
        let no_hints = SequenceHeader::default();
        assert_eq!(relative_dist(&no_hints, 10, 4), 0);
    }

    /// `set_frame_refs()` (7.8), which no encoder the clips come from uses: worked by hand.
    #[test]
    fn short_signaling_fills_the_references() {
        let s = sequence();
        let refs = |hints: [u32; NUM_REF_FRAMES], last: u8, gold: u8| {
            let mut h = FrameHeader {
                order_hint: 10,
                last_frame_idx: last,
                gold_frame_idx: gold,
                ..FrameHeader::default()
            };
            set_frame_refs(&s, &mut h, &hints);
            h.ref_frame_idx
        };
        // Backward slots 4 (16, the latest: ALTREF), 6 (11, the earliest: BWDREF), 3 (12:
        // ALTREF2); LAST2 and LAST3 from the latest forward ones, 1 (8) and 5 (6).
        assert_eq!(
            refs([9, 8, 7, 12, 16, 6, 11, 5], 0, 2),
            [0, 1, 5, 2, 6, 3, 4]
        );
        // No forward slot left: LAST2 and LAST3 take the earliest of all, slot 1 (8).
        assert_eq!(
            refs([9, 8, 11, 12, 13, 14, 15, 16], 0, 1),
            [0, 1, 1, 1, 2, 3, 7]
        );
        // Equal hints: the later slot wins a tie for the latest.
        assert_eq!(refs([9; NUM_REF_FRAMES], 0, 1), [0, 7, 6, 1, 5, 4, 3]);
    }

    #[test]
    fn subexponential_codes_recenter_on_the_reference() {
        // inverse_recenter: values up to twice the reference alternate around it.
        assert_eq!(inverse_recenter(5, 0), 5);
        assert_eq!(inverse_recenter(5, 1), 4);
        assert_eq!(inverse_recenter(5, 2), 6);
        assert_eq!(inverse_recenter(5, 11), 11);
        // A damaged reference does not panic.
        assert_eq!(inverse_recenter(u32::MAX, 4), 1);
    }
}
