//! Builds the vendored libopus 1.5.2 (third_party/opus, BSD-3-Clause) as one static
//! library: floating point, always optimized (decoding in a debug build would be too slow
//! to stream music), with its SSE, SSE2 and SSE4.1 code used directly on x86-64 (the
//! engine's baseline, x86-64-v2, has them; AVX2 would need runtime dispatch and is left
//! out). The source lists follow upstream's `celt_sources.mk`, `silk_sources.mk` and
//! `opus_sources.mk`.

// A build script has no caller to return errors to: a missing vendored tree must stop the
// build with its path.
#![allow(clippy::expect_used)]

use std::path::PathBuf;

const CELT: &[&str] = &[
    "bands.c",
    "celt.c",
    "celt_encoder.c",
    "celt_decoder.c",
    "cwrs.c",
    "entcode.c",
    "entdec.c",
    "entenc.c",
    "kiss_fft.c",
    "laplace.c",
    "mathops.c",
    "mdct.c",
    "modes.c",
    "pitch.c",
    "celt_lpc.c",
    "quant_bands.c",
    "rate.c",
    "vq.c",
];
const CELT_X86: &[&str] = &[
    "x86/pitch_sse.c",
    "x86/pitch_sse2.c",
    "x86/vq_sse2.c",
    "x86/celt_lpc_sse4_1.c",
    "x86/pitch_sse4_1.c",
];
const SILK: &[&str] = &[
    "CNG.c",
    "code_signs.c",
    "init_decoder.c",
    "decode_core.c",
    "decode_frame.c",
    "decode_parameters.c",
    "decode_indices.c",
    "decode_pulses.c",
    "decoder_set_fs.c",
    "dec_API.c",
    "enc_API.c",
    "encode_indices.c",
    "encode_pulses.c",
    "gain_quant.c",
    "interpolate.c",
    "LP_variable_cutoff.c",
    "NLSF_decode.c",
    "NSQ.c",
    "NSQ_del_dec.c",
    "PLC.c",
    "shell_coder.c",
    "tables_gain.c",
    "tables_LTP.c",
    "tables_NLSF_CB_NB_MB.c",
    "tables_NLSF_CB_WB.c",
    "tables_other.c",
    "tables_pitch_lag.c",
    "tables_pulses_per_block.c",
    "VAD.c",
    "control_audio_bandwidth.c",
    "quant_LTP_gains.c",
    "VQ_WMat_EC.c",
    "HP_variable_cutoff.c",
    "NLSF_encode.c",
    "NLSF_VQ.c",
    "NLSF_unpack.c",
    "NLSF_del_dec_quant.c",
    "process_NLSFs.c",
    "stereo_LR_to_MS.c",
    "stereo_MS_to_LR.c",
    "check_control_input.c",
    "control_SNR.c",
    "init_encoder.c",
    "control_codec.c",
    "A2NLSF.c",
    "ana_filt_bank_1.c",
    "biquad_alt.c",
    "bwexpander_32.c",
    "bwexpander.c",
    "debug.c",
    "decode_pitch.c",
    "inner_prod_aligned.c",
    "lin2log.c",
    "log2lin.c",
    "LPC_analysis_filter.c",
    "LPC_inv_pred_gain.c",
    "table_LSF_cos.c",
    "NLSF2A.c",
    "NLSF_stabilize.c",
    "NLSF_VQ_weights_laroia.c",
    "pitch_est_tables.c",
    "resampler.c",
    "resampler_down2_3.c",
    "resampler_down2.c",
    "resampler_private_AR2.c",
    "resampler_private_down_FIR.c",
    "resampler_private_IIR_FIR.c",
    "resampler_private_up2_HQ.c",
    "resampler_rom.c",
    "sigm_Q15.c",
    "sort.c",
    "sum_sqr_shift.c",
    "stereo_decode_pred.c",
    "stereo_encode_pred.c",
    "stereo_find_predictor.c",
    "stereo_quant_pred.c",
    "LPC_fit.c",
];
const SILK_X86: &[&str] = &[
    "x86/NSQ_sse4_1.c",
    "x86/NSQ_del_dec_sse4_1.c",
    "x86/VAD_sse4_1.c",
    "x86/VQ_WMat_EC_sse4_1.c",
];
const SILK_FLOAT: &[&str] = &[
    "apply_sine_window_FLP.c",
    "corrMatrix_FLP.c",
    "encode_frame_FLP.c",
    "find_LPC_FLP.c",
    "find_LTP_FLP.c",
    "find_pitch_lags_FLP.c",
    "find_pred_coefs_FLP.c",
    "LPC_analysis_filter_FLP.c",
    "LTP_analysis_filter_FLP.c",
    "LTP_scale_ctrl_FLP.c",
    "noise_shape_analysis_FLP.c",
    "process_gains_FLP.c",
    "regularize_correlations_FLP.c",
    "residual_energy_FLP.c",
    "warped_autocorrelation_FLP.c",
    "wrappers_FLP.c",
    "autocorrelation_FLP.c",
    "burg_modified_FLP.c",
    "bwexpander_FLP.c",
    "energy_FLP.c",
    "inner_product_FLP.c",
    "k2a_FLP.c",
    "LPC_inv_pred_gain_FLP.c",
    "pitch_analysis_core_FLP.c",
    "scale_copy_vector_FLP.c",
    "scale_vector_FLP.c",
    "schur_FLP.c",
    "sort_FLP.c",
];
const OPUS: &[&str] = &[
    "opus.c",
    "opus_decoder.c",
    "opus_encoder.c",
    "extensions.c",
    "opus_multistream.c",
    "opus_multistream_encoder.c",
    "opus_multistream_decoder.c",
    "repacketizer.c",
    "opus_projection_encoder.c",
    "opus_projection_decoder.c",
    "mapping_matrix.c",
    "analysis.c",
    "mlp.c",
    "mlp_data.c",
];

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../third_party/opus");
    assert!(
        root.join("include/opus.h").exists(),
        "vendored libopus missing at {}",
        root.display()
    );
    let x86 = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    let mut build = cc::Build::new();
    build
        .opt_level(3)
        .debug(false)
        .warnings(false)
        .include(&root)
        .include(root.join("include"))
        .include(root.join("celt"))
        .include(root.join("silk"))
        .include(root.join("silk/float"))
        .include(root.join("src"))
        // What upstream's config.h would say for a float build.
        .define("OPUS_BUILD", None)
        .define("USE_ALLOCA", None);
    if !msvc {
        build
            .define("HAVE_ALLOCA_H", "1")
            .define("HAVE_LRINTF", "1")
            .define("HAVE_LRINT", "1");
    }
    if x86 {
        // Presumed, not detected: the functions call their SSE versions directly and no
        // runtime CPU detection is compiled in.
        for isa in ["SSE", "SSE2", "SSE4_1"] {
            build.define(&format!("OPUS_X86_MAY_HAVE_{isa}"), None);
            build.define(&format!("OPUS_X86_PRESUME_{isa}"), None);
        }
        if !msvc {
            build.flag("-msse4.1");
        }
    }

    let groups: [(&str, &[&str], bool); 6] = [
        ("celt", CELT, true),
        ("celt", CELT_X86, x86),
        ("silk", SILK, true),
        ("silk", SILK_X86, x86),
        ("silk/float", SILK_FLOAT, true),
        ("src", OPUS, true),
    ];
    for (dir, files, enabled) in groups {
        if !enabled {
            continue;
        }
        for file in files {
            build.file(root.join(dir).join(file));
        }
    }
    build.compile("opus");
    println!("cargo:rerun-if-changed={}", root.display());
}
