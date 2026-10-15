//! The few libopus functions the decoder calls (declared from `include/opus.h`).

use core::ffi::{c_int, c_uchar};

/// libopus's decoder state (opaque).
#[repr(C)]
pub(crate) struct OpusDecoder {
    _private: [u8; 0],
}

pub(crate) const OPUS_OK: c_int = 0;
/// `opus_decoder_ctl` request that resets the decoder's state (as after a seek).
pub(crate) const OPUS_RESET_STATE: c_int = 4028;

unsafe extern "C" {
    pub(crate) fn opus_decoder_create(
        fs: i32,
        channels: c_int,
        error: *mut c_int,
    ) -> *mut OpusDecoder;
    pub(crate) fn opus_decoder_destroy(st: *mut OpusDecoder);
    pub(crate) fn opus_decode_float(
        st: *mut OpusDecoder,
        data: *const c_uchar,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    pub(crate) fn opus_decoder_ctl(st: *mut OpusDecoder, request: c_int, ...) -> c_int;
    pub(crate) fn opus_packet_get_nb_samples(packet: *const c_uchar, len: i32, fs: i32) -> c_int;
}
