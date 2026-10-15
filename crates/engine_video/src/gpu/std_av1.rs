//! The parsed AV1 headers as Vulkan Video's `StdVideoAV1*` structures (the Vulkan
//! specification's "AV1 Decoding Parameters" and its Video Std header
//! `vulkan_video_codec_av1std_decode`).
//!
//! The structures point to each other, so each set lives in one boxed value that does not
//! move, filled in place for every frame (nothing is allocated per frame).

use engine_rhi::ash::vk::native::{
    StdVideoAV1CDEF, StdVideoAV1ColorConfig, StdVideoAV1ColorConfigFlags, StdVideoAV1FilmGrain,
    StdVideoAV1FilmGrainFlags, StdVideoAV1GlobalMotion, StdVideoAV1LoopFilter,
    StdVideoAV1LoopFilterFlags, StdVideoAV1LoopRestoration, StdVideoAV1Quantization,
    StdVideoAV1QuantizationFlags, StdVideoAV1Segmentation, StdVideoAV1SequenceHeader,
    StdVideoAV1SequenceHeaderFlags, StdVideoAV1TileInfo, StdVideoAV1TileInfoFlags,
    StdVideoAV1TimingInfo, StdVideoAV1TimingInfoFlags, StdVideoDecodeAV1PictureInfo,
    StdVideoDecodeAV1PictureInfoFlags, StdVideoDecodeAV1ReferenceInfo,
    StdVideoDecodeAV1ReferenceInfoFlags,
};

use crate::av1::frame::{FrameHeader, LAST_FRAME};
use crate::av1::{MAX_SEGMENTS, MAX_TILE_COLS, MAX_TILE_ROWS, REFS_PER_FRAME, SequenceHeader};

/// A sequence header and the structures it points to.
pub(crate) struct StdSequence {
    color: StdVideoAV1ColorConfig,
    timing: StdVideoAV1TimingInfo,
    /// What `VkVideoDecodeAV1SessionParametersCreateInfoKHR` points to.
    pub(crate) header: StdVideoAV1SequenceHeader,
}

/// The struct initializers below spell every field: the `StdVideo*` types have no
/// `Default` in ash.
fn zeroed<T: Copy>() -> T {
    // SAFETY: only used for the plain `StdVideo*` structures (integers, arrays, bitfield
    // units and raw pointers), for which all-zero bytes are a valid value.
    unsafe { core::mem::zeroed() }
}

impl StdSequence {
    pub(crate) fn new(s: &SequenceHeader) -> Box<Self> {
        let c = &s.color;
        let mut color_flags = StdVideoAV1ColorConfigFlags {
            _bitfield_align_1: [],
            _bitfield_1: zeroed(),
        };
        color_flags.set_mono_chrome(c.mono_chrome.into());
        color_flags.set_color_range(c.color_range.into());
        color_flags.set_separate_uv_delta_q(c.separate_uv_delta_q.into());
        color_flags.set_color_description_present_flag(c.color_description_present_flag.into());
        let color = StdVideoAV1ColorConfig {
            flags: color_flags,
            BitDepth: c.bit_depth,
            subsampling_x: c.subsampling_x.into(),
            subsampling_y: c.subsampling_y.into(),
            reserved1: 0,
            color_primaries: c.color_primaries.into(),
            transfer_characteristics: c.transfer_characteristics.into(),
            matrix_coefficients: c.matrix_coefficients.into(),
            chroma_sample_position: c.chroma_sample_position.into(),
        };
        let mut timing_flags = StdVideoAV1TimingInfoFlags {
            _bitfield_align_1: [],
            _bitfield_1: zeroed(),
        };
        let timing_info = s.timing_info.unwrap_or_default();
        timing_flags.set_equal_picture_interval(timing_info.equal_picture_interval.into());
        let timing = StdVideoAV1TimingInfo {
            flags: timing_flags,
            num_units_in_display_tick: timing_info.num_units_in_display_tick,
            time_scale: timing_info.time_scale,
            num_ticks_per_picture_minus_1: timing_info.num_ticks_per_picture_minus_1,
        };
        let mut flags = StdVideoAV1SequenceHeaderFlags {
            _bitfield_align_1: [],
            _bitfield_1: zeroed(),
        };
        flags.set_still_picture(s.still_picture.into());
        flags.set_reduced_still_picture_header(s.reduced_still_picture_header.into());
        flags.set_use_128x128_superblock(s.use_128x128_superblock.into());
        flags.set_enable_filter_intra(s.enable_filter_intra.into());
        flags.set_enable_intra_edge_filter(s.enable_intra_edge_filter.into());
        flags.set_enable_interintra_compound(s.enable_interintra_compound.into());
        flags.set_enable_masked_compound(s.enable_masked_compound.into());
        flags.set_enable_warped_motion(s.enable_warped_motion.into());
        flags.set_enable_dual_filter(s.enable_dual_filter.into());
        flags.set_enable_order_hint(s.enable_order_hint.into());
        flags.set_enable_jnt_comp(s.enable_jnt_comp.into());
        flags.set_enable_ref_frame_mvs(s.enable_ref_frame_mvs.into());
        flags.set_frame_id_numbers_present_flag(s.frame_id_numbers_present_flag.into());
        flags.set_enable_superres(s.enable_superres.into());
        flags.set_enable_cdef(s.enable_cdef.into());
        flags.set_enable_restoration(s.enable_restoration.into());
        flags.set_film_grain_params_present(s.film_grain_params_present.into());
        flags.set_timing_info_present_flag(s.timing_info.is_some().into());
        flags.set_initial_display_delay_present_flag(s.initial_display_delay_present_flag.into());
        let mut sequence = Box::new(Self {
            color,
            timing,
            header: StdVideoAV1SequenceHeader {
                flags,
                seq_profile: s.seq_profile.into(),
                frame_width_bits_minus_1: s.frame_width_bits_minus_1,
                frame_height_bits_minus_1: s.frame_height_bits_minus_1,
                max_frame_width_minus_1: s.max_frame_width_minus_1 as u16,
                max_frame_height_minus_1: s.max_frame_height_minus_1 as u16,
                delta_frame_id_length_minus_2: s.delta_frame_id_length_minus_2,
                additional_frame_id_length_minus_1: s.additional_frame_id_length_minus_1,
                order_hint_bits_minus_1: s.order_hint_bits_minus_1,
                seq_force_integer_mv: s.seq_force_integer_mv,
                seq_force_screen_content_tools: s.seq_force_screen_content_tools,
                reserved1: [0; 5],
                pColorConfig: core::ptr::null(),
                pTimingInfo: core::ptr::null(),
            },
        });
        sequence.header.pColorConfig = &raw const sequence.color;
        sequence.header.pTimingInfo = if s.timing_info.is_some() {
            &raw const sequence.timing
        } else {
            core::ptr::null()
        };
        sequence
    }
}

/// A frame's picture information and the structures it points to.
pub(crate) struct StdPicture {
    mi_col_starts: [u16; MAX_TILE_COLS + 1],
    mi_row_starts: [u16; MAX_TILE_ROWS + 1],
    width_in_sbs_minus_1: [u16; MAX_TILE_COLS],
    height_in_sbs_minus_1: [u16; MAX_TILE_ROWS],
    tile_info: StdVideoAV1TileInfo,
    quantization: StdVideoAV1Quantization,
    segmentation: StdVideoAV1Segmentation,
    loop_filter: StdVideoAV1LoopFilter,
    cdef: StdVideoAV1CDEF,
    restoration: StdVideoAV1LoopRestoration,
    global_motion: StdVideoAV1GlobalMotion,
    film_grain: StdVideoAV1FilmGrain,
    /// What `VkVideoDecodeAV1PictureInfoKHR::pStdPictureInfo` points to.
    pub(crate) info: StdVideoDecodeAV1PictureInfo,
}

impl StdPicture {
    pub(crate) fn new() -> Box<Self> {
        Box::new(Self {
            mi_col_starts: [0; MAX_TILE_COLS + 1],
            mi_row_starts: [0; MAX_TILE_ROWS + 1],
            width_in_sbs_minus_1: [0; MAX_TILE_COLS],
            height_in_sbs_minus_1: [0; MAX_TILE_ROWS],
            tile_info: zeroed(),
            quantization: zeroed(),
            segmentation: zeroed(),
            loop_filter: zeroed(),
            cdef: zeroed(),
            restoration: zeroed(),
            global_motion: zeroed(),
            film_grain: zeroed(),
            info: zeroed(),
        })
    }

    /// Fills the structures from frame header `h`; the tiles' starts in superblocks of
    /// `1 << starts_in_superblocks` mode info units when given (see [`tile_starts_quirk`]),
    /// in mode info units otherwise, as the specification says. Film grain is applied when the stream asks for it and `apply_grain` allows it (a
    /// profile without film grain support must not enable it).
    pub(crate) fn fill(
        &mut self,
        h: &FrameHeader,
        starts_in_superblocks: Option<u32>,
        apply_grain: bool,
    ) {
        let t = &h.tile_info;
        let (cols, rows) = (t.tile_cols as usize, t.tile_rows as usize);
        self.mi_col_starts[..=cols].copy_from_slice(&t.mi_col_starts[..=cols]);
        self.mi_row_starts[..=rows].copy_from_slice(&t.mi_row_starts[..=rows]);
        if let Some(shift) = starts_in_superblocks {
            // The superblock a tile starts at; the last entry the superblocks across.
            for v in self.mi_col_starts[..=cols]
                .iter_mut()
                .chain(&mut self.mi_row_starts[..=rows])
            {
                *v = (*v + (1 << shift) - 1) >> shift;
            }
        }
        self.width_in_sbs_minus_1[..cols].copy_from_slice(&t.width_in_sbs_minus_1[..cols]);
        self.height_in_sbs_minus_1[..rows].copy_from_slice(&t.height_in_sbs_minus_1[..rows]);
        let mut tile_flags = StdVideoAV1TileInfoFlags {
            _bitfield_align_1: [],
            _bitfield_1: zeroed(),
        };
        tile_flags.set_uniform_tile_spacing_flag(t.uniform_tile_spacing_flag.into());
        self.tile_info = StdVideoAV1TileInfo {
            flags: tile_flags,
            TileCols: t.tile_cols as u8,
            TileRows: t.tile_rows as u8,
            context_update_tile_id: t.context_update_tile_id as u16,
            tile_size_bytes_minus_1: t.tile_size_bytes_minus_1,
            reserved1: [0; 7],
            pMiColStarts: self.mi_col_starts.as_ptr(),
            pMiRowStarts: self.mi_row_starts.as_ptr(),
            pWidthInSbsMinus1: self.width_in_sbs_minus_1.as_ptr(),
            pHeightInSbsMinus1: self.height_in_sbs_minus_1.as_ptr(),
        };

        let q = &h.quantization;
        let mut q_flags = StdVideoAV1QuantizationFlags {
            _bitfield_align_1: [],
            _bitfield_1: zeroed(),
        };
        q_flags.set_using_qmatrix(q.using_qmatrix.into());
        q_flags.set_diff_uv_delta(q.diff_uv_delta.into());
        self.quantization = StdVideoAV1Quantization {
            flags: q_flags,
            base_q_idx: q.base_q_idx,
            DeltaQYDc: q.delta_q_y_dc,
            DeltaQUDc: q.delta_q_u_dc,
            DeltaQUAc: q.delta_q_u_ac,
            DeltaQVDc: q.delta_q_v_dc,
            DeltaQVAc: q.delta_q_v_ac,
            qm_y: q.qm_y,
            qm_u: q.qm_u,
            qm_v: q.qm_v,
        };

        // FeatureEnabled: a bitmask of features per segment.
        let g = &h.segmentation;
        for i in 0..MAX_SEGMENTS {
            self.segmentation.FeatureEnabled[i] = g.feature_enabled[i]
                .iter()
                .enumerate()
                .fold(0u8, |mask, (j, &on)| mask | u8::from(on) << j);
            self.segmentation.FeatureData[i] = g.feature_data[i];
        }

        let l = &h.loop_filter;
        let mut lf_flags = StdVideoAV1LoopFilterFlags {
            _bitfield_align_1: [],
            _bitfield_1: zeroed(),
        };
        lf_flags.set_loop_filter_delta_enabled(l.delta_enabled.into());
        lf_flags.set_loop_filter_delta_update(l.delta_update.into());
        self.loop_filter = StdVideoAV1LoopFilter {
            flags: lf_flags,
            loop_filter_level: l.level,
            loop_filter_sharpness: l.sharpness,
            update_ref_delta: l.update_ref_delta,
            loop_filter_ref_deltas: l.ref_deltas,
            update_mode_delta: l.update_mode_delta,
            loop_filter_mode_deltas: l.mode_deltas,
        };

        // Secondary strengths as coded (3 stays 3).
        let c = &h.cdef;
        self.cdef = StdVideoAV1CDEF {
            cdef_damping_minus_3: c.damping_minus_3,
            cdef_bits: c.bits,
            cdef_y_pri_strength: c.y_pri_strength,
            cdef_y_sec_strength: c.y_sec_strength,
            cdef_uv_pri_strength: c.uv_pri_strength,
            cdef_uv_sec_strength: c.uv_sec_strength,
        };

        // LoopRestorationSize as log2(size) - 5.
        let r = &h.restoration;
        for plane in 0..3 {
            self.restoration.FrameRestorationType[plane] = r.frame_restoration_type[plane].into();
            let size = r.loop_restoration_size[plane];
            self.restoration.LoopRestorationSize[plane] = if size > 0 {
                (size.trailing_zeros() as u16).saturating_sub(5)
            } else {
                0
            };
        }

        self.global_motion = StdVideoAV1GlobalMotion {
            GmType: h.global_motion.gm_type,
            gm_params: h.global_motion.gm_params,
        };

        let f = &h.film_grain;
        let grain = apply_grain && f.apply_grain;
        let mut fg_flags = StdVideoAV1FilmGrainFlags {
            _bitfield_align_1: [],
            _bitfield_1: zeroed(),
        };
        fg_flags.set_chroma_scaling_from_luma(f.chroma_scaling_from_luma.into());
        fg_flags.set_overlap_flag(f.overlap_flag.into());
        fg_flags.set_clip_to_restricted_range(f.clip_to_restricted_range.into());
        // Ignored by the decoder: the effective parameters are given (the parser loaded
        // a reference's when the frame did not update them).
        fg_flags.set_update_grain(f.update_grain.into());
        let bytes = |values: &[u8]| -> [i8; 25] {
            let mut out = [0i8; 25];
            for (o, &v) in out.iter_mut().zip(values) {
                *o = v as i8;
            }
            out
        };
        let y = bytes(&f.ar_coeffs_y_plus_128);
        let mut ar_coeffs_y_plus_128 = [0i8; 24];
        ar_coeffs_y_plus_128.copy_from_slice(&y[..24]);
        self.film_grain = StdVideoAV1FilmGrain {
            flags: fg_flags,
            grain_scaling_minus_8: f.grain_scaling_minus_8,
            ar_coeff_lag: f.ar_coeff_lag,
            ar_coeff_shift_minus_6: f.ar_coeff_shift_minus_6,
            grain_scale_shift: f.grain_scale_shift,
            grain_seed: f.grain_seed,
            film_grain_params_ref_idx: f.film_grain_params_ref_idx,
            num_y_points: f.num_y_points,
            point_y_value: f.point_y_value,
            point_y_scaling: f.point_y_scaling,
            num_cb_points: f.num_cb_points,
            point_cb_value: f.point_cb_value,
            point_cb_scaling: f.point_cb_scaling,
            num_cr_points: f.num_cr_points,
            point_cr_value: f.point_cr_value,
            point_cr_scaling: f.point_cr_scaling,
            ar_coeffs_y_plus_128,
            ar_coeffs_cb_plus_128: bytes(&f.ar_coeffs_cb_plus_128),
            ar_coeffs_cr_plus_128: bytes(&f.ar_coeffs_cr_plus_128),
            cb_mult: f.cb_mult,
            cb_luma_mult: f.cb_luma_mult,
            cb_offset: f.cb_offset,
            cr_mult: f.cr_mult,
            cr_luma_mult: f.cr_luma_mult,
            cr_offset: f.cr_offset,
        };

        let mut flags = StdVideoDecodeAV1PictureInfoFlags {
            _bitfield_align_1: [],
            _bitfield_1: zeroed(),
        };
        flags.set_error_resilient_mode(h.error_resilient_mode.into());
        flags.set_disable_cdf_update(h.disable_cdf_update.into());
        flags.set_use_superres(h.use_superres.into());
        flags.set_render_and_frame_size_different(h.render_and_frame_size_different.into());
        flags.set_allow_screen_content_tools(h.allow_screen_content_tools.into());
        flags.set_is_filter_switchable(h.is_filter_switchable.into());
        flags.set_force_integer_mv(h.force_integer_mv.into());
        flags.set_frame_size_override_flag(h.frame_size_override_flag.into());
        flags.set_buffer_removal_time_present_flag(h.buffer_removal_time_present_flag.into());
        flags.set_allow_intrabc(h.allow_intrabc.into());
        flags.set_frame_refs_short_signaling(h.frame_refs_short_signaling.into());
        flags.set_allow_high_precision_mv(h.allow_high_precision_mv.into());
        flags.set_is_motion_mode_switchable(h.is_motion_mode_switchable.into());
        flags.set_use_ref_frame_mvs(h.use_ref_frame_mvs.into());
        flags.set_disable_frame_end_update_cdf(h.disable_frame_end_update_cdf.into());
        flags.set_allow_warped_motion(h.allow_warped_motion.into());
        flags.set_reduced_tx_set(h.reduced_tx_set.into());
        flags.set_reference_select(h.reference_select.into());
        flags.set_skip_mode_present(h.skip_mode_present.into());
        flags.set_delta_q_present(h.deltas.delta_q_present.into());
        flags.set_delta_lf_present(h.deltas.delta_lf_present.into());
        flags.set_delta_lf_multi(h.deltas.delta_lf_multi.into());
        flags.set_segmentation_enabled(g.enabled.into());
        flags.set_segmentation_update_map(g.update_map.into());
        flags.set_segmentation_temporal_update(g.temporal_update.into());
        flags.set_segmentation_update_data(g.update_data.into());
        flags.set_UsesLr(r.uses_lr.into());
        flags.set_usesChromaLr(r.uses_chroma_lr.into());
        flags.set_apply_grain(grain.into());

        let mut order_hints = [0u8; 8];
        for (o, &hint) in order_hints.iter_mut().zip(&h.order_hints) {
            *o = hint as u8;
        }
        // expectedFrameId[i] for the references i = 0..REFS_PER_FRAME (section 6.8.2).
        let mut expected = [0u32; 8];
        expected[..REFS_PER_FRAME].copy_from_slice(&h.expected_frame_id);
        self.info = StdVideoDecodeAV1PictureInfo {
            flags,
            frame_type: h.frame_type as u32,
            current_frame_id: h.current_frame_id,
            OrderHint: h.order_hint as u8,
            primary_ref_frame: h.primary_ref_frame,
            refresh_frame_flags: h.refresh_frame_flags,
            reserved1: 0,
            interpolation_filter: h.interpolation_filter.into(),
            TxMode: h.tx_mode.into(),
            delta_q_res: h.deltas.delta_q_res,
            delta_lf_res: h.deltas.delta_lf_res,
            SkipModeFrame: h.skip_mode_frame,
            coded_denom: h.coded_denom,
            reserved2: [0; 3],
            OrderHints: order_hints,
            expectedFrameId: expected,
            pTileInfo: &raw const self.tile_info,
            pQuantization: &raw const self.quantization,
            pSegmentation: &raw const self.segmentation,
            pLoopFilter: &raw const self.loop_filter,
            // Read by the decoder only when the sequence or the frame uses them, but never
            // null: zeros where unused.
            pCDEF: &raw const self.cdef,
            pLoopRestoration: &raw const self.restoration,
            pGlobalMotion: &raw const self.global_motion,
            pFilmGrain: &raw const self.film_grain,
        };
    }
}

/// Whether a driver reads `pMiColStarts` and `pMiRowStarts` in superblocks.
///
/// The specification has them in mode info units and advises drivers to ignore them
/// (applications sent either). AMD's PAL-based drivers read them as superblocks: given
/// mode info units, every tile but the first decodes wrong (seen on the Radeon 780M,
/// driver 26.8.1, 2026-10-16). NVIDIA ignores them; Mesa's RADV derives the starts from
/// `pWidthInSbsMinus1` and the frame size.
pub(crate) fn tile_starts_quirk(driver: engine_rhi::ash::vk::DriverId) -> bool {
    use engine_rhi::ash::vk::DriverId;
    driver == DriverId::AMD_PROPRIETARY || driver == DriverId::AMD_OPEN_SOURCE
}

/// What a decoded frame leaves in its reference slot: Vulkan's reference information.
pub(crate) fn reference_info(h: &FrameHeader) -> StdVideoDecodeAV1ReferenceInfo {
    let mut flags = StdVideoDecodeAV1ReferenceInfoFlags {
        _bitfield_align_1: [],
        _bitfield_1: zeroed(),
    };
    flags.set_disable_frame_end_update_cdf(h.disable_frame_end_update_cdf.into());
    flags.set_segmentation_enabled(h.segmentation.enabled.into());
    let mut saved = [0u8; 8];
    for (o, &hint) in saved.iter_mut().zip(&h.order_hints) {
        *o = hint as u8;
    }
    let sign_bias = h.ref_frame_sign_bias[LAST_FRAME..]
        .iter()
        .enumerate()
        .fold(0u8, |mask, (i, &bias)| {
            mask | u8::from(bias) << (i + LAST_FRAME)
        });
    StdVideoDecodeAV1ReferenceInfo {
        flags,
        frame_type: h.frame_type as u8,
        RefFrameSignBias: sign_bias,
        OrderHint: h.order_hint as u8,
        SavedOrderHints: saved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1::frame::FrameType;

    #[test]
    fn pointers_stay_inside_the_boxes() {
        let sequence = SequenceHeader {
            enable_cdef: true,
            ..SequenceHeader::default()
        };
        let std_sequence = StdSequence::new(&sequence);
        assert_eq!(
            std_sequence.header.pColorConfig,
            &raw const std_sequence.color
        );
        assert!(std_sequence.header.pTimingInfo.is_null());

        let mut header = FrameHeader {
            frame_type: FrameType::Inter,
            ..FrameHeader::default()
        };
        header.restoration.uses_lr = true;
        header.restoration.loop_restoration_size = [256, 128, 64];
        header.segmentation.feature_enabled[2][0] = true;
        header.segmentation.feature_enabled[2][7] = true;
        header.ref_frame_sign_bias[LAST_FRAME + 4] = true;
        let mut picture = StdPicture::new();
        picture.fill(&header, None, false);
        assert_eq!(picture.info.pTileInfo, &raw const picture.tile_info);
        assert_eq!(picture.info.pCDEF, &raw const picture.cdef);
        assert_eq!(picture.info.pFilmGrain, &raw const picture.film_grain);
        // log2(size) - 5.
        assert_eq!(picture.restoration.LoopRestorationSize, [3, 2, 1]);
        assert_eq!(picture.segmentation.FeatureEnabled[2], 0b1000_0001);
        assert_eq!(
            reference_info(&header).RefFrameSignBias,
            1 << (LAST_FRAME + 4)
        );
        assert_eq!(picture.info.flags.UsesLr(), 1);
    }
}
