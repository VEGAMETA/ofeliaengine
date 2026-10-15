//! The clips in `tests/clips` (their README says how they were made): the WebM reader
//! finds their tracks, blocks and cues, and the AV1 parser's headers match rav1d's for
//! every frame, shown or hidden, with tiles where the frame has them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use engine_video::av1::frame::{FrameHeader, LAST_FRAME};
use engine_video::av1::{Parser, SequenceHeader, TemporalUnit};
use engine_video::software::{Settings, SoftwareDecoder};
use engine_video::webm::{Codec, TrackKind, Webm};
use rav1d::include::dav1d::headers::{Dav1dFrameHeader, Dav1dSequenceHeader};

struct Clip {
    name: &'static str,
    width: u32,
    height: u32,
    video_blocks: usize,
    audio_blocks: usize,
    bit_depth: u8,
    /// Whether the encoder was asked for several tiles.
    tiles: bool,
    film_grain: bool,
}

const CLIPS: [Clip; 9] = [
    Clip {
        name: "svt_8bit_320x180.webm",
        width: 320,
        height: 180,
        video_blocks: 60,
        audio_blocks: 101,
        bit_depth: 8,
        tiles: false,
        film_grain: false,
    },
    Clip {
        name: "aom_10bit_tiles_320x180.webm",
        width: 320,
        height: 180,
        video_blocks: 60,
        audio_blocks: 0,
        bit_depth: 10,
        tiles: true,
        film_grain: false,
    },
    Clip {
        name: "svt_grain_tiles_640x360.webm",
        width: 640,
        height: 360,
        video_blocks: 30,
        audio_blocks: 0,
        bit_depth: 8,
        tiles: true,
        film_grain: true,
    },
    Clip {
        name: "rav1e_8bit_320x180.webm",
        width: 320,
        height: 180,
        video_blocks: 30,
        audio_blocks: 0,
        bit_depth: 8,
        tiles: false,
        film_grain: false,
    },
    Clip {
        name: "aom_tools_320x180.webm",
        width: 320,
        height: 180,
        video_blocks: 21,
        audio_blocks: 0,
        bit_depth: 8,
        tiles: false,
        film_grain: true,
    },
    Clip {
        name: "aom_resilient_320x180.webm",
        width: 320,
        height: 180,
        video_blocks: 21,
        audio_blocks: 0,
        bit_depth: 8,
        tiles: false,
        film_grain: false,
    },
    Clip {
        name: "aom_lossless_160x90.webm",
        width: 160,
        height: 90,
        video_blocks: 6,
        audio_blocks: 0,
        bit_depth: 8,
        tiles: false,
        film_grain: false,
    },
    Clip {
        name: "svt_superres_320x180.webm",
        width: 320,
        height: 180,
        video_blocks: 21,
        audio_blocks: 0,
        bit_depth: 8,
        tiles: false,
        film_grain: false,
    },
    Clip {
        name: "svt_resize_320x180.webm",
        width: 320,
        height: 180,
        video_blocks: 21,
        audio_blocks: 0,
        bit_depth: 8,
        tiles: false,
        film_grain: false,
    },
];

fn read(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/clips/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

#[test]
fn webm_tracks_blocks_and_cues() {
    for clip in &CLIPS {
        let data = read(clip.name);
        let webm = Webm::read(&data).unwrap();
        let video = webm.track(TrackKind::Video).unwrap();
        assert_eq!(video.codec, Codec::Av1, "{}", clip.name);
        assert_eq!(
            (video.width, video.height),
            (clip.width, clip.height),
            "{}",
            clip.name
        );
        assert!(!video.private.is_empty(), "{}: no av1C record", clip.name);
        let audio = webm.track(TrackKind::Audio);
        if clip.audio_blocks > 0 {
            let audio = audio.unwrap();
            assert_eq!(audio.codec, Codec::Opus);
            assert_eq!(audio.sample_rate, 48_000.0);
            assert_eq!(audio.channels, 2);
            // OpusHead, with its pre-skip as the codec delay.
            assert!(audio.private.starts_with(b"OpusHead"));
            assert!(audio.codec_delay_ns > 0);
        }
        let (mut video_blocks, mut audio_blocks) = (0, 0);
        let mut last = i64::MIN;
        for block in webm.blocks() {
            let block = block.unwrap();
            if block.track == video.number {
                if video_blocks == 0 {
                    assert!(
                        block.keyframe,
                        "{}: the first block is not a keyframe",
                        clip.name
                    );
                }
                assert!(
                    block.time_ns > last,
                    "{}: video blocks out of order",
                    clip.name
                );
                last = block.time_ns;
                video_blocks += 1;
            } else {
                audio_blocks += block.frame_count();
            }
        }
        assert_eq!(video_blocks, clip.video_blocks, "{}", clip.name);
        assert_eq!(audio_blocks, clip.audio_blocks, "{}", clip.name);
        // Every cue points at a keyframe of its track, at its time.
        assert!(!webm.cues.is_empty(), "{}: no cues", clip.name);
        for cue in &webm.cues {
            let block = webm
                .blocks_from(cue.cluster_offset)
                .map(Result::unwrap)
                .find(|b| b.track == cue.track && b.time_ns as u64 == cue.time_ns)
                .unwrap_or_else(|| panic!("{}: no block at the cue {cue:?}", clip.name));
            assert!(
                block.keyframe,
                "{}: a cue's block is not a keyframe",
                clip.name
            );
        }
    }
}

/// Compares numbers of any width, collecting the differences.
macro_rules! same {
    ($errors:expr, $what:expr, $ours:expr, $theirs:expr) => {{
        let (ours, theirs) = (i64::from($ours), i64::from($theirs));
        if ours != theirs {
            $errors.push(format!("{}: ours {ours}, rav1d's {theirs}", $what));
        }
    }};
}

fn compare_sequence(s: &SequenceHeader, d: &Dav1dSequenceHeader, e: &mut Vec<String>) {
    let c = &s.color;
    same!(e, "profile", s.seq_profile, d.profile);
    same!(e, "max_width", s.max_width(), d.max_width);
    same!(e, "max_height", s.max_height(), d.max_height);
    let layout = match (c.mono_chrome, c.subsampling_x, c.subsampling_y) {
        (true, ..) => 0,
        (false, true, true) => 1,
        (false, true, false) => 2,
        _ => 3,
    };
    same!(e, "layout", layout, d.layout);
    same!(e, "color_primaries", c.color_primaries, d.pri);
    same!(
        e,
        "transfer_characteristics",
        c.transfer_characteristics,
        d.trc
    );
    same!(e, "matrix_coefficients", c.matrix_coefficients, d.mtrx);
    same!(e, "chroma_sample_position", c.chroma_sample_position, d.chr);
    same!(e, "bit_depth", (c.bit_depth - 8) / 2, d.hbd);
    same!(e, "color_range", c.color_range, d.color_range);
    same!(
        e,
        "operating points",
        s.operating_points_count as u32,
        d.num_operating_points
    );
    for (i, (ours, theirs)) in s.operating_points[..s.operating_points_count]
        .iter()
        .zip(&d.operating_points)
        .enumerate()
    {
        same!(e, format!("operating_point_idc[{i}]"), ours.idc, theirs.idc);
        same!(
            e,
            format!("seq_level_idx[{i}] major"),
            2 + (ours.seq_level_idx >> 2),
            theirs.major_level
        );
        same!(
            e,
            format!("seq_level_idx[{i}] minor"),
            ours.seq_level_idx & 3,
            theirs.minor_level
        );
        same!(e, format!("seq_tier[{i}]"), ours.seq_tier, theirs.tier);
        same!(
            e,
            format!("decoder_model_present[{i}]"),
            ours.decoder_model_present,
            theirs.decoder_model_param_present
        );
        same!(
            e,
            format!("initial_display_delay_present[{i}]"),
            ours.initial_display_delay_present,
            theirs.display_model_param_present
        );
    }
    same!(e, "still_picture", s.still_picture, d.still_picture);
    same!(
        e,
        "reduced_still_picture_header",
        s.reduced_still_picture_header,
        d.reduced_still_picture_header
    );
    same!(
        e,
        "timing_info_present",
        s.timing_info.is_some(),
        d.timing_info_present
    );
    if let Some(t) = s.timing_info {
        same!(
            e,
            "num_units_in_display_tick",
            t.num_units_in_display_tick,
            d.num_units_in_tick
        );
        same!(e, "time_scale", t.time_scale, d.time_scale);
        same!(
            e,
            "equal_picture_interval",
            t.equal_picture_interval,
            d.equal_picture_interval
        );
        if t.equal_picture_interval {
            same!(
                e,
                "num_ticks_per_picture",
                t.num_ticks_per_picture_minus_1 + 1,
                d.num_ticks_per_picture
            );
        }
    }
    same!(
        e,
        "decoder_model_info_present",
        s.decoder_model_info.is_some(),
        d.decoder_model_info_present
    );
    same!(
        e,
        "frame_width_bits",
        s.frame_width_bits_minus_1 + 1,
        d.width_n_bits
    );
    same!(
        e,
        "frame_height_bits",
        s.frame_height_bits_minus_1 + 1,
        d.height_n_bits
    );
    same!(
        e,
        "frame_id_numbers_present",
        s.frame_id_numbers_present_flag,
        d.frame_id_numbers_present
    );
    if s.frame_id_numbers_present_flag {
        same!(
            e,
            "delta_frame_id_length",
            s.delta_frame_id_length_minus_2 + 2,
            d.delta_frame_id_n_bits
        );
        same!(e, "frame_id_length", s.frame_id_length(), d.frame_id_n_bits);
    }
    same!(
        e,
        "use_128x128_superblock",
        s.use_128x128_superblock,
        d.sb128
    );
    same!(
        e,
        "enable_filter_intra",
        s.enable_filter_intra,
        d.filter_intra
    );
    same!(
        e,
        "enable_intra_edge_filter",
        s.enable_intra_edge_filter,
        d.intra_edge_filter
    );
    same!(
        e,
        "enable_interintra_compound",
        s.enable_interintra_compound,
        d.inter_intra
    );
    same!(
        e,
        "enable_masked_compound",
        s.enable_masked_compound,
        d.masked_compound
    );
    same!(
        e,
        "enable_warped_motion",
        s.enable_warped_motion,
        d.warped_motion
    );
    same!(e, "enable_dual_filter", s.enable_dual_filter, d.dual_filter);
    same!(e, "enable_order_hint", s.enable_order_hint, d.order_hint);
    same!(e, "enable_jnt_comp", s.enable_jnt_comp, d.jnt_comp);
    same!(
        e,
        "enable_ref_frame_mvs",
        s.enable_ref_frame_mvs,
        d.ref_frame_mvs
    );
    same!(
        e,
        "seq_force_screen_content_tools",
        s.seq_force_screen_content_tools,
        d.screen_content_tools
    );
    same!(
        e,
        "seq_force_integer_mv",
        s.seq_force_integer_mv,
        d.force_integer_mv
    );
    same!(e, "order_hint_bits", s.order_hint_bits, d.order_hint_n_bits);
    same!(e, "enable_superres", s.enable_superres, d.super_res);
    same!(e, "enable_cdef", s.enable_cdef, d.cdef);
    same!(e, "enable_restoration", s.enable_restoration, d.restoration);
    same!(e, "subsampling_x", c.subsampling_x, d.ss_hor);
    same!(e, "subsampling_y", c.subsampling_y, d.ss_ver);
    same!(e, "mono_chrome", c.mono_chrome, d.monochrome);
    same!(
        e,
        "color_description_present",
        c.color_description_present_flag,
        d.color_description_present
    );
    same!(
        e,
        "separate_uv_delta_q",
        c.separate_uv_delta_q,
        d.separate_uv_delta_q
    );
    same!(
        e,
        "film_grain_params_present",
        s.film_grain_params_present,
        d.film_grain_present
    );
}

fn compare_frame(s: &SequenceHeader, h: &FrameHeader, d: &Dav1dFrameHeader, e: &mut Vec<String>) {
    let inter = !h.is_intra();
    same!(e, "frame_type", h.frame_type as u8, d.frame_type);
    same!(e, "FrameWidth", h.frame_width, d.width[0]);
    same!(e, "UpscaledWidth", h.upscaled_width, d.width[1]);
    same!(e, "FrameHeight", h.frame_height, d.height);
    same!(e, "order_hint", h.order_hint, d.frame_offset);
    same!(e, "temporal_id", h.temporal_id, d.temporal_id);
    same!(e, "spatial_id", h.spatial_id, d.spatial_id);
    same!(
        e,
        "show_existing_frame",
        h.show_existing_frame,
        d.show_existing_frame
    );
    same!(e, "current_frame_id", h.current_frame_id, d.frame_id);
    same!(e, "show_frame", h.show_frame, d.show_frame);
    same!(e, "showable_frame", h.showable_frame, d.showable_frame);
    same!(
        e,
        "error_resilient_mode",
        h.error_resilient_mode,
        d.error_resilient_mode
    );
    same!(
        e,
        "disable_cdf_update",
        h.disable_cdf_update,
        d.disable_cdf_update
    );
    same!(
        e,
        "allow_screen_content_tools",
        h.allow_screen_content_tools,
        d.allow_screen_content_tools
    );
    same!(
        e,
        "force_integer_mv",
        h.force_integer_mv,
        d.force_integer_mv
    );
    same!(
        e,
        "frame_size_override_flag",
        h.frame_size_override_flag,
        d.frame_size_override
    );
    same!(
        e,
        "primary_ref_frame",
        h.primary_ref_frame,
        d.primary_ref_frame
    );
    same!(
        e,
        "buffer_removal_time_present",
        h.buffer_removal_time_present_flag,
        d.buffer_removal_time_present
    );
    same!(
        e,
        "refresh_frame_flags",
        h.refresh_frame_flags,
        d.refresh_frame_flags
    );
    same!(e, "RenderWidth", h.render_width, d.render_width);
    same!(e, "RenderHeight", h.render_height, d.render_height);
    same!(
        e,
        "SuperresDenom",
        h.superres_denom,
        d.super_res.width_scale_denominator
    );
    same!(e, "use_superres", h.use_superres, d.super_res.enabled);
    same!(
        e,
        "render_and_frame_size_different",
        h.render_and_frame_size_different,
        d.have_render_size
    );
    same!(e, "allow_intrabc", h.allow_intrabc, d.allow_intrabc);
    if inter {
        same!(
            e,
            "frame_refs_short_signaling",
            h.frame_refs_short_signaling,
            d.frame_ref_short_signaling
        );
        for i in 0..7 {
            same!(
                e,
                format!("ref_frame_idx[{i}]"),
                h.ref_frame_idx[i],
                d.refidx[i]
            );
        }
        same!(
            e,
            "allow_high_precision_mv",
            h.allow_high_precision_mv,
            d.hp
        );
        same!(
            e,
            "interpolation_filter",
            h.interpolation_filter,
            d.subpel_filter_mode
        );
        same!(
            e,
            "is_motion_mode_switchable",
            h.is_motion_mode_switchable,
            d.switchable_motion_mode
        );
    }
    same!(
        e,
        "use_ref_frame_mvs",
        h.use_ref_frame_mvs,
        d.use_ref_frame_mvs
    );
    same!(
        e,
        "disable_frame_end_update_cdf",
        !h.disable_frame_end_update_cdf,
        d.refresh_context
    );

    let t = &h.tile_info;
    let sb_shift = if s.use_128x128_superblock { 5 } else { 4 };
    same!(
        e,
        "uniform_tile_spacing_flag",
        t.uniform_tile_spacing_flag,
        d.tiling.uniform
    );
    same!(e, "TileCols", t.tile_cols, d.tiling.cols);
    same!(e, "TileRows", t.tile_rows, d.tiling.rows);
    same!(e, "TileColsLog2", t.tile_cols_log2, d.tiling.log2_cols);
    same!(e, "TileRowsLog2", t.tile_rows_log2, d.tiling.log2_rows);
    // rav1d's starts are in superblocks, the last one the superblocks across the frame.
    let sb = |mi: u16| (mi + (1 << sb_shift) - 1) >> sb_shift;
    for i in 0..=t.tile_cols as usize {
        same!(
            e,
            format!("MiColStarts[{i}]"),
            sb(t.mi_col_starts[i]),
            d.tiling.col_start_sb[i]
        );
    }
    for i in 0..=t.tile_rows as usize {
        same!(
            e,
            format!("MiRowStarts[{i}]"),
            sb(t.mi_row_starts[i]),
            d.tiling.row_start_sb[i]
        );
    }
    same!(
        e,
        "context_update_tile_id",
        t.context_update_tile_id,
        d.tiling.update
    );
    if t.count() > 1 {
        same!(e, "TileSizeBytes", t.size_bytes(), d.tiling.n_bytes);
    }

    let q = &h.quantization;
    same!(e, "base_q_idx", q.base_q_idx, d.quant.yac);
    same!(e, "DeltaQYDc", q.delta_q_y_dc, d.quant.ydc_delta);
    same!(e, "DeltaQUDc", q.delta_q_u_dc, d.quant.udc_delta);
    same!(e, "DeltaQUAc", q.delta_q_u_ac, d.quant.uac_delta);
    same!(e, "DeltaQVDc", q.delta_q_v_dc, d.quant.vdc_delta);
    same!(e, "DeltaQVAc", q.delta_q_v_ac, d.quant.vac_delta);
    same!(e, "using_qmatrix", q.using_qmatrix, d.quant.qm);
    if q.using_qmatrix {
        same!(e, "qm_y", q.qm_y, d.quant.qm_y);
        same!(e, "qm_u", q.qm_u, d.quant.qm_u);
        same!(e, "qm_v", q.qm_v, d.quant.qm_v);
    }

    let g = &h.segmentation;
    same!(e, "segmentation_enabled", g.enabled, d.segmentation.enabled);
    if g.enabled {
        same!(
            e,
            "segmentation_update_map",
            g.update_map,
            d.segmentation.update_map
        );
        same!(
            e,
            "segmentation_temporal_update",
            g.temporal_update,
            d.segmentation.temporal
        );
        same!(
            e,
            "segmentation_update_data",
            g.update_data,
            d.segmentation.update_data
        );
        for i in 0..8 {
            let theirs = &d.segmentation.seg_data.d[i];
            let data = |j: usize| {
                if g.feature_enabled[i][j] {
                    g.feature_data[i][j]
                } else {
                    0
                }
            };
            same!(
                e,
                format!("FeatureData[{i}][ALT_Q]"),
                data(0),
                theirs.delta_q
            );
            same!(
                e,
                format!("FeatureData[{i}][ALT_LF_Y_V]"),
                data(1),
                theirs.delta_lf_y_v
            );
            same!(
                e,
                format!("FeatureData[{i}][ALT_LF_Y_H]"),
                data(2),
                theirs.delta_lf_y_h
            );
            same!(
                e,
                format!("FeatureData[{i}][ALT_LF_U]"),
                data(3),
                theirs.delta_lf_u
            );
            same!(
                e,
                format!("FeatureData[{i}][ALT_LF_V]"),
                data(4),
                theirs.delta_lf_v
            );
            let reference = if g.feature_enabled[i][5] {
                g.feature_data[i][5]
            } else {
                -1
            };
            same!(
                e,
                format!("FeatureData[{i}][REF_FRAME]"),
                reference,
                theirs.r#ref
            );
            same!(
                e,
                format!("FeatureEnabled[{i}][SKIP]"),
                g.feature_enabled[i][6],
                theirs.skip
            );
            same!(
                e,
                format!("FeatureEnabled[{i}][GLOBALMV]"),
                g.feature_enabled[i][7],
                theirs.globalmv
            );
        }
        same!(
            e,
            "SegIdPreSkip",
            g.seg_id_pre_skip,
            d.segmentation.seg_data.preskip
        );
        same!(
            e,
            "LastActiveSegId",
            g.last_active_seg_id,
            d.segmentation.seg_data.last_active_segid.max(0)
        );
    }
    for i in 0..8 {
        same!(
            e,
            format!("LosslessArray[{i}]"),
            h.lossless_array[i],
            d.segmentation.lossless[i]
        );
    }

    same!(
        e,
        "delta_q_present",
        h.deltas.delta_q_present,
        d.delta.q.present
    );
    same!(e, "delta_q_res", h.deltas.delta_q_res, d.delta.q.res_log2);
    same!(
        e,
        "delta_lf_present",
        h.deltas.delta_lf_present,
        d.delta.lf.present
    );
    same!(
        e,
        "delta_lf_res",
        h.deltas.delta_lf_res,
        d.delta.lf.res_log2
    );
    same!(
        e,
        "delta_lf_multi",
        h.deltas.delta_lf_multi,
        d.delta.lf.multi
    );
    // rav1d's all_lossless is CodedLossless.
    same!(e, "CodedLossless", h.coded_lossless, d.all_lossless);

    let l = &h.loop_filter;
    same!(
        e,
        "loop_filter_level[0]",
        l.level[0],
        d.loopfilter.level_y[0]
    );
    same!(
        e,
        "loop_filter_level[1]",
        l.level[1],
        d.loopfilter.level_y[1]
    );
    same!(e, "loop_filter_level[2]", l.level[2], d.loopfilter.level_u);
    same!(e, "loop_filter_level[3]", l.level[3], d.loopfilter.level_v);
    same!(
        e,
        "loop_filter_sharpness",
        l.sharpness,
        d.loopfilter.sharpness
    );
    same!(
        e,
        "loop_filter_delta_enabled",
        l.delta_enabled,
        d.loopfilter.mode_ref_delta_enabled
    );
    same!(
        e,
        "loop_filter_delta_update",
        l.delta_update,
        d.loopfilter.mode_ref_delta_update
    );
    for i in 0..8 {
        same!(
            e,
            format!("loop_filter_ref_deltas[{i}]"),
            l.ref_deltas[i],
            d.loopfilter.mode_ref_deltas.ref_delta[i]
        );
    }
    for i in 0..2 {
        same!(
            e,
            format!("loop_filter_mode_deltas[{i}]"),
            l.mode_deltas[i],
            d.loopfilter.mode_ref_deltas.mode_delta[i]
        );
    }

    let c = &h.cdef;
    if s.enable_cdef && !h.coded_lossless && !h.allow_intrabc {
        same!(e, "CdefDamping", c.damping_minus_3 + 3, d.cdef.damping);
        same!(e, "cdef_bits", c.bits, d.cdef.n_bits);
        for i in 0..1usize << c.bits {
            same!(
                e,
                format!("cdef_y_strength[{i}]"),
                c.y_pri_strength[i] << 2 | c.y_sec_strength[i],
                d.cdef.y_strength[i]
            );
            if s.color.num_planes() > 1 {
                same!(
                    e,
                    format!("cdef_uv_strength[{i}]"),
                    c.uv_pri_strength[i] << 2 | c.uv_sec_strength[i],
                    d.cdef.uv_strength[i]
                );
            }
        }
    }

    let r = &h.restoration;
    // Ours are FrameRestorationType (none, Wiener, self-guided, switchable); rav1d's the
    // coded lr_type (none, switchable, Wiener, self-guided).
    const CODED: [u8; 4] = [0, 2, 3, 1];
    for plane in 0..s.color.num_planes() as usize {
        same!(
            e,
            format!("lr_type[{plane}]"),
            CODED[usize::from(r.frame_restoration_type[plane])],
            d.restoration.r#type[plane]
        );
    }
    if r.uses_lr {
        same!(
            e,
            "LoopRestorationSize[0]",
            r.loop_restoration_size[0].trailing_zeros(),
            d.restoration.unit_size[0]
        );
    }
    if r.uses_chroma_lr {
        same!(
            e,
            "LoopRestorationSize[1]",
            r.loop_restoration_size[1].trailing_zeros(),
            d.restoration.unit_size[1]
        );
    }

    same!(e, "TxMode", h.tx_mode, d.txfm_mode);
    same!(
        e,
        "reference_select",
        h.reference_select,
        d.switchable_comp_refs
    );
    same!(
        e,
        "skip_mode_present",
        h.skip_mode_present,
        d.skip_mode_enabled
    );
    if h.skip_mode_present {
        for i in 0..2 {
            same!(
                e,
                format!("SkipModeFrame[{i}]"),
                h.skip_mode_frame[i] - LAST_FRAME as u8,
                d.skip_mode_refs[i]
            );
        }
    }
    same!(
        e,
        "allow_warped_motion",
        h.allow_warped_motion,
        d.warp_motion
    );
    same!(e, "reduced_tx_set", h.reduced_tx_set, d.reduced_txtp_set);
    if inter {
        for i in 0..7 {
            let reference = LAST_FRAME + i;
            same!(
                e,
                format!("GmType[{reference}]"),
                h.global_motion.gm_type[reference],
                d.gmv[i].r#type
            );
            for j in 0..6 {
                same!(
                    e,
                    format!("gm_params[{reference}][{j}]"),
                    h.global_motion.gm_params[reference][j],
                    d.gmv[i].matrix[j]
                );
            }
        }
    }

    let f = &h.film_grain;
    let g = &d.film_grain.data;
    same!(e, "apply_grain", f.apply_grain, d.film_grain.present);
    if f.apply_grain {
        same!(e, "update_grain", f.update_grain, d.film_grain.update);
        same!(e, "grain_seed", f.grain_seed, g.seed);
        same!(e, "num_y_points", f.num_y_points, g.num_y_points);
        for i in 0..usize::from(f.num_y_points) {
            same!(
                e,
                format!("point_y_value[{i}]"),
                f.point_y_value[i],
                g.y_points[i][0]
            );
            same!(
                e,
                format!("point_y_scaling[{i}]"),
                f.point_y_scaling[i],
                g.y_points[i][1]
            );
        }
        same!(
            e,
            "chroma_scaling_from_luma",
            f.chroma_scaling_from_luma,
            g.chroma_scaling_from_luma
        );
        same!(e, "num_cb_points", f.num_cb_points, g.num_uv_points[0]);
        same!(e, "num_cr_points", f.num_cr_points, g.num_uv_points[1]);
        for i in 0..usize::from(f.num_cb_points) {
            same!(
                e,
                format!("point_cb_value[{i}]"),
                f.point_cb_value[i],
                g.uv_points[0][i][0]
            );
            same!(
                e,
                format!("point_cb_scaling[{i}]"),
                f.point_cb_scaling[i],
                g.uv_points[0][i][1]
            );
        }
        for i in 0..usize::from(f.num_cr_points) {
            same!(
                e,
                format!("point_cr_value[{i}]"),
                f.point_cr_value[i],
                g.uv_points[1][i][0]
            );
            same!(
                e,
                format!("point_cr_scaling[{i}]"),
                f.point_cr_scaling[i],
                g.uv_points[1][i][1]
            );
        }
        same!(
            e,
            "grain_scaling",
            f.grain_scaling_minus_8 + 8,
            g.scaling_shift
        );
        same!(e, "ar_coeff_lag", f.ar_coeff_lag, g.ar_coeff_lag);
        let lag = usize::from(f.ar_coeff_lag);
        let luma = 2 * lag * (lag + 1);
        if f.num_y_points > 0 {
            for i in 0..luma {
                same!(
                    e,
                    format!("ar_coeffs_y[{i}]"),
                    i16::from(f.ar_coeffs_y_plus_128[i]) - 128,
                    g.ar_coeffs_y[i]
                );
            }
        }
        let chroma = luma + usize::from(f.num_y_points > 0);
        if f.chroma_scaling_from_luma || f.num_cb_points > 0 {
            for i in 0..chroma {
                same!(
                    e,
                    format!("ar_coeffs_cb[{i}]"),
                    i16::from(f.ar_coeffs_cb_plus_128[i]) - 128,
                    g.ar_coeffs_uv[0][i]
                );
            }
        }
        if f.chroma_scaling_from_luma || f.num_cr_points > 0 {
            for i in 0..chroma {
                same!(
                    e,
                    format!("ar_coeffs_cr[{i}]"),
                    i16::from(f.ar_coeffs_cr_plus_128[i]) - 128,
                    g.ar_coeffs_uv[1][i]
                );
            }
        }
        same!(
            e,
            "ar_coeff_shift",
            u32::from(f.ar_coeff_shift_minus_6) + 6,
            g.ar_coeff_shift as u32
        );
        same!(
            e,
            "grain_scale_shift",
            f.grain_scale_shift,
            g.grain_scale_shift
        );
        if f.num_cb_points > 0 {
            same!(e, "cb_mult", i32::from(f.cb_mult) - 128, g.uv_mult[0]);
            same!(
                e,
                "cb_luma_mult",
                i32::from(f.cb_luma_mult) - 128,
                g.uv_luma_mult[0]
            );
            same!(e, "cb_offset", i32::from(f.cb_offset) - 256, g.uv_offset[0]);
        }
        if f.num_cr_points > 0 {
            same!(e, "cr_mult", i32::from(f.cr_mult) - 128, g.uv_mult[1]);
            same!(
                e,
                "cr_luma_mult",
                i32::from(f.cr_luma_mult) - 128,
                g.uv_luma_mult[1]
            );
            same!(e, "cr_offset", i32::from(f.cr_offset) - 256, g.uv_offset[1]);
        }
        same!(e, "overlap_flag", f.overlap_flag, g.overlap_flag);
        same!(
            e,
            "clip_to_restricted_range",
            f.clip_to_restricted_range,
            g.clip_to_restricted_range
        );
    }
}

/// What the clips' frames use, so the comparison is known to have covered it.
fn note_features(s: &SequenceHeader, h: &FrameHeader, seen: &mut BTreeMap<&'static str, usize>) {
    let t = &h.tile_info;
    let q = &h.quantization;
    for (feature, used) in [
        ("hidden frames", !h.show_frame),
        ("10 bits", s.color.bit_depth == 10),
        ("error resilient frames", h.error_resilient_mode),
        ("frame ids", s.frame_id_numbers_present_flag),
        ("frame size override", h.frame_size_override_flag),
        ("super-resolution", h.use_superres),
        ("render size", h.render_and_frame_size_different),
        ("screen content tools", h.allow_screen_content_tools),
        ("intra block copy", h.allow_intrabc),
        ("tiles", t.count() > 1),
        ("quantizer matrices", q.using_qmatrix),
        (
            "chroma quantizer deltas",
            q.delta_q_u_dc != 0 || q.delta_q_u_ac != 0,
        ),
        (
            "segmentation data",
            h.segmentation.enabled && h.segmentation.update_data,
        ),
        ("delta q", h.deltas.delta_q_present),
        ("delta loop filter", h.deltas.delta_lf_present),
        ("lossless", h.coded_lossless),
        ("CDEF strengths", h.cdef.bits > 0),
        ("loop restoration", h.restoration.uses_lr),
        ("chroma loop restoration", h.restoration.uses_chroma_lr),
        ("transform size select", h.tx_mode == 2),
        ("compound references", h.reference_select),
        ("skip mode", h.skip_mode_present),
        ("warped motion", h.allow_warped_motion),
        (
            "global motion",
            h.global_motion.gm_type[LAST_FRAME..]
                .iter()
                .any(|&t| t != 0),
        ),
        (
            "switchable interpolation",
            !h.is_intra() && h.is_filter_switchable,
        ),
        ("reference motion vectors", h.use_ref_frame_mvs),
        ("primary reference", h.primary_ref_frame != 7),
        ("film grain", h.film_grain.apply_grain),
    ] {
        *seen.entry(feature).or_default() += usize::from(used);
    }
}

#[test]
fn av1_headers_match_rav1d() {
    let (mut existing, mut multi_frame_units) = (0, 0);
    let mut seen = BTreeMap::new();
    for clip in &CLIPS {
        let data = read(clip.name);
        let webm = Webm::read(&data).unwrap();
        let video = webm.track(TrackKind::Video).unwrap();
        // rav1e's av1C record has no configuration OBUs: its first block brings the
        // sequence header.
        let mut parser = Parser::with_codec_private(&video.private).unwrap();
        // One frame at a time, hidden frames output too: a picture per parsed frame.
        let mut decoder = SoftwareDecoder::new(&Settings {
            threads: 1,
            max_frame_delay: 1,
            apply_grain: false,
            output_invisible_frames: true,
        })
        .unwrap();
        let mut unit = TemporalUnit::default();
        let (mut frames, mut grain, mut most_tiles) = (0, 0, 0);
        for block in webm.blocks() {
            let block = block.unwrap();
            if block.track != video.number {
                continue;
            }
            let at = format!("{} at {} ms", clip.name, block.time_ns / 1_000_000);
            parser
                .parse(block.data(), &mut unit)
                .unwrap_or_else(|error| panic!("{at}: {error}"));
            let mut pictures = Vec::new();
            decoder
                .decode(block.data(), block.time_ns, |picture| {
                    pictures.push(picture)
                })
                .unwrap_or_else(|error| panic!("{at}: {error}"));
            assert_eq!(unit.frames.len(), pictures.len(), "{at}: frames");
            multi_frame_units += usize::from(unit.frames.len() > 1);
            let sequence = parser.sequence().unwrap();
            assert_eq!(sequence.color.bit_depth, clip.bit_depth, "{at}");
            for (frame, picture) in unit.frames.iter().zip(&pictures) {
                let h = &frame.header;
                let theirs = picture.frame_header().unwrap();
                let mut errors = Vec::new();
                compare_sequence(sequence, picture.sequence_header().unwrap(), &mut errors);
                if h.show_existing_frame {
                    // rav1d outputs the shown frame's picture with the header it was decoded with.
                    existing += 1;
                    same!(
                        errors,
                        "shown frame's order_hint",
                        h.order_hint,
                        theirs.frame_offset
                    );
                    same!(
                        errors,
                        "shown frame's frame_type",
                        h.frame_type as u8,
                        theirs.frame_type
                    );
                    assert_eq!(
                        unit.tile_offsets(frame).len(),
                        0,
                        "{at}: tiles of a shown existing frame"
                    );
                } else {
                    compare_frame(sequence, h, theirs, &mut errors);
                    note_features(sequence, h, &mut seen);
                    grain += usize::from(h.film_grain.apply_grain);
                    // The tiles: all of them, in order, inside the unit.
                    let (offsets, sizes) = (unit.tile_offsets(frame), unit.tile_sizes(frame));
                    assert_eq!(offsets.len() as u32, h.tile_info.count(), "{at}: tiles");
                    most_tiles = most_tiles.max(offsets.len());
                    let mut end = frame.header_offset;
                    for (&offset, &size) in offsets.iter().zip(sizes) {
                        assert!(offset >= end && size > 0, "{at}: tiles out of order");
                        end = offset + size;
                    }
                    assert!(
                        end as usize <= block.data().len(),
                        "{at}: a tile past the unit"
                    );
                }
                assert!(
                    errors.is_empty(),
                    "{at}, frame {frames}:\n{}",
                    errors.join("\n")
                );
                frames += 1;
            }
        }
        decoder
            .finish(|_| panic!("{}: a picture after the last unit", clip.name))
            .unwrap();
        assert!(frames >= clip.video_blocks, "{}", clip.name);
        assert_eq!(grain > 0, clip.film_grain, "{}: film grain", clip.name);
        assert_eq!(most_tiles > 1, clip.tiles, "{}: tiles", clip.name);
    }
    // Every feature above was compared at least once, and frames shown again and units of
    // several frames were seen. Not in any clip (their encoders do not make them): intra-only
    // and switch frames, short reference signaling (a unit test of set_frame_refs), tile
    // sizes not uniform, loop filter deltas updated, film grain loaded from a reference or
    // scaled from luma.
    let unused: Vec<_> = seen
        .iter()
        .filter(|&(_, &n)| n == 0)
        .map(|(f, _)| *f)
        .collect();
    assert!(unused.is_empty(), "no clip has: {}", unused.join(", "));
    assert!(
        existing > 0 && multi_frame_units > 0,
        "{existing} {multi_frame_units}"
    );
}
