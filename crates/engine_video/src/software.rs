//! Decoding on the CPU with rav1d (BSD-2-Clause, the Rust port of dav1d): the fallback
//! where the GPU has no AV1 decoder, and the reference the GPU's pictures are tested
//! against (ADR 0100).
//!
//! rav1d's interface is dav1d's C interface; this module keeps its `unsafe` in one place.

use core::ffi::c_int;
use core::fmt;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422, Dav1dFrameHeader,
    Dav1dSequenceHeader,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush,
    dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
};

/// `DAV1D_ERR(EAGAIN)`: the Windows CRT's `EAGAIN`, negated.
const AGAIN: c_int = -11;
/// `DAV1D_ERR(ENOMEM)`.
const NO_MEMORY: c_int = -12;
/// `DAV1D_ERR(EINVAL)`.
const INVALID: c_int = -22;
/// rav1d's limit on worker threads.
const MAX_THREADS: u32 = 256;

/// Why rav1d could not decode: its negated `errno` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError(pub c_int);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            NO_MEMORY => f.write_str("rav1d ran out of memory"),
            INVALID => f.write_str("rav1d found the AV1 data invalid"),
            // ENOPROTOOPT: a stream rav1d does not decode (a bit depth not built in).
            -42 => f.write_str("rav1d does not decode this AV1 stream"),
            code => write!(f, "rav1d failed (error {code})"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn check(result: c_int) -> Result<(), DecodeError> {
    if result < 0 {
        Err(DecodeError(result))
    } else {
        Ok(())
    }
}

/// The decoder's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Worker threads; 0 lets rav1d choose from the logical cores.
    pub threads: u32,
    /// Frames decoded at once; 0 lets rav1d choose from the threads, 1 decodes one at a
    /// time (the lowest latency).
    pub max_frame_delay: u32,
    /// Whether the output pictures carry the stream's film grain.
    pub apply_grain: bool,
    /// Whether frames the stream does not show are output too (for tests).
    pub output_invisible_frames: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threads: 0,
            max_frame_delay: 0,
            apply_grain: true,
            output_invisible_frames: false,
        }
    }
}

/// The chroma layout of a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Luma only.
    I400,
    /// Chroma at half the width and height.
    I420,
    /// Chroma at half the width.
    I422,
    /// Chroma at full resolution.
    I444,
}

/// An AV1 decoder on the CPU.
pub struct SoftwareDecoder {
    context: Dav1dContext,
    /// Whether rav1d decodes several frames at once: asking it for a picture twice
    /// without new data then waits for the frames in flight.
    frame_threads: bool,
}

// SAFETY: a dav1d context may be used from any thread, one thread at a time, which
// `&mut self` on every call guarantees; its workers synchronize with it themselves.
unsafe impl Send for SoftwareDecoder {}

impl fmt::Debug for SoftwareDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SoftwareDecoder").finish_non_exhaustive()
    }
}

impl SoftwareDecoder {
    /// Opens a decoder.
    ///
    /// # Errors
    ///
    /// rav1d could not start (out of memory or threads).
    pub fn new(settings: &Settings) -> Result<Self, DecodeError> {
        let mut defaults = MaybeUninit::<Dav1dSettings>::uninit();
        // SAFETY: `defaults` is valid for writes; rav1d writes the whole structure.
        unsafe { dav1d_default_settings(NonNull::from(&mut defaults).cast()) };
        // SAFETY: written in full just above.
        let mut s = unsafe { defaults.assume_init() };
        s.n_threads = settings.threads.min(MAX_THREADS) as c_int;
        s.max_frame_delay = settings.max_frame_delay.min(MAX_THREADS) as c_int;
        s.apply_grain = c_int::from(settings.apply_grain);
        s.output_invisible_frames = c_int::from(settings.output_invisible_frames);
        let mut context: Option<Dav1dContext> = None;
        // SAFETY: both pointers are valid for the call; rav1d copies the settings.
        let result = unsafe {
            dav1d_open(
                Some(NonNull::from(&mut context)),
                Some(NonNull::from(&mut s)),
            )
        };
        check(result.0)?;
        // rav1d decodes one frame at a time with one thread or a frame delay of 1.
        let frame_threads = settings.threads != 1 && settings.max_frame_delay != 1;
        context
            .map(|context| Self {
                context,
                frame_threads,
            })
            .ok_or(DecodeError(INVALID))
    }

    /// Decodes a temporal unit (a WebM block's data), handing `output` the pictures it
    /// completes, in output order: decoding one frame at a time, all of the unit's; with
    /// frame threading, those ready (the rest come with later units, or [`Self::finish`]).
    /// `timestamp` comes back on the unit's pictures.
    ///
    /// # Errors
    ///
    /// Data rav1d cannot decode.
    pub fn decode(
        &mut self,
        data: &[u8],
        timestamp: i64,
        mut output: impl FnMut(Picture),
    ) -> Result<(), DecodeError> {
        if data.is_empty() {
            return Ok(());
        }
        let mut buffer = Dav1dData::default();
        // SAFETY: `buffer` is valid; rav1d allocates `data.len()` bytes that it owns.
        let target = unsafe { dav1d_data_create(Some(NonNull::from(&mut buffer)), data.len()) };
        if target.is_null() {
            return Err(DecodeError(NO_MEMORY));
        }
        // SAFETY: `target` has room for `data.len()` bytes and is not part of `data`.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), target, data.len()) };
        buffer.m.timestamp = timestamp;
        // dav1d's loop: send the data (refused while a finished picture waits), collect
        // pictures, until rav1d holds the data.
        let result = loop {
            // SAFETY: the context is open; `buffer` holds data rav1d created.
            let sent =
                unsafe { dav1d_send_data(Some(self.context), Some(NonNull::from(&mut buffer))) }.0;
            if sent < 0 && sent != AGAIN {
                break Err(DecodeError(sent));
            }
            if let Err(error) = self.collect(&mut output) {
                break Err(error);
            }
            if buffer.sz == 0 {
                break Ok(());
            }
        };
        // SAFETY: releases whatever rav1d did not take (nothing, once it took it all).
        unsafe { dav1d_data_unref(Some(NonNull::from(&mut buffer))) };
        result
    }

    /// At the end of a stream: hands `output` the pictures still being decoded.
    ///
    /// # Errors
    ///
    /// A frame in flight that rav1d could not decode.
    pub fn finish(&mut self, mut output: impl FnMut(Picture)) -> Result<(), DecodeError> {
        // Called again and again without data, dav1d_get_picture drains its frames.
        while let Some(picture) = self.picture()? {
            output(picture);
        }
        Ok(())
    }

    /// Drops the frames in flight and the stream's state (for a seek).
    pub fn flush(&mut self) {
        // SAFETY: the context is open.
        unsafe { dav1d_flush(self.context) };
    }

    /// Hands `output` the pictures rav1d has ready. Each request parses more of the data
    /// rav1d holds, so one frame at a time that is every picture of it; with frame
    /// threading, a second request would wait for the frames in flight: one at most.
    fn collect(&mut self, output: &mut impl FnMut(Picture)) -> Result<(), DecodeError> {
        while let Some(picture) = self.picture()? {
            output(picture);
            if self.frame_threads {
                break;
            }
        }
        Ok(())
    }

    fn picture(&mut self) -> Result<Option<Picture>, DecodeError> {
        let mut picture = Dav1dPicture::default();
        // SAFETY: the context is open; `picture` is valid for rav1d to fill.
        let result =
            unsafe { dav1d_get_picture(Some(self.context), Some(NonNull::from(&mut picture))) }.0;
        match result {
            AGAIN => Ok(None),
            0 => Ok(Some(Picture { picture })),
            error => Err(DecodeError(error)),
        }
    }
}

impl Drop for SoftwareDecoder {
    fn drop(&mut self) {
        let mut context = Some(self.context);
        // SAFETY: the context is open and not used after this; rav1d joins its threads.
        unsafe { dav1d_close(Some(NonNull::from(&mut context))) };
    }
}

/// A decoded picture: rav1d's planes, held until dropped.
pub struct Picture {
    picture: Dav1dPicture,
}

// SAFETY: a picture is reference counted and read only once decoded; rav1d releases it
// from any thread.
unsafe impl Send for Picture {}

impl fmt::Debug for Picture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Picture")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("bit_depth", &self.bit_depth())
            .field("layout", &self.layout())
            .finish_non_exhaustive()
    }
}

impl Picture {
    /// The width in luma samples (after super-resolution).
    pub fn width(&self) -> u32 {
        self.picture.p.w as u32
    }

    /// The height in luma samples.
    pub fn height(&self) -> u32 {
        self.picture.p.h as u32
    }

    /// Bits per sample: 8, 10 or 12.
    pub fn bit_depth(&self) -> u32 {
        self.picture.p.bpc as u32
    }

    /// The chroma layout.
    pub fn layout(&self) -> Layout {
        match self.picture.p.layout {
            DAV1D_PIXEL_LAYOUT_I400 => Layout::I400,
            DAV1D_PIXEL_LAYOUT_I420 => Layout::I420,
            DAV1D_PIXEL_LAYOUT_I422 => Layout::I422,
            _ => Layout::I444,
        }
    }

    /// The timestamp given with the temporal unit the picture came from.
    pub fn timestamp(&self) -> i64 {
        self.picture.m.timestamp
    }

    /// A plane's size in samples (0: luma, 1 and 2: chroma).
    pub fn plane_size(&self, plane: usize) -> (u32, u32) {
        let (w, h) = (self.width(), self.height());
        match (plane, self.layout()) {
            (0, _) => (w, h),
            (_, Layout::I420) => (w.div_ceil(2), h.div_ceil(2)),
            (_, Layout::I422) => (w.div_ceil(2), h),
            _ => (w, h),
        }
    }

    /// A plane's rows and the bytes from one row to the next. Samples are bytes at 8 bits
    /// and little-endian 16-bit words above. `None` for chroma of a luma-only picture.
    pub fn plane(&self, plane: usize) -> Option<(&[u8], usize)> {
        if plane > 2 || (plane > 0 && self.layout() == Layout::I400) {
            return None;
        }
        let data = self.picture.data[plane]?;
        let stride = usize::try_from(self.picture.stride[usize::from(plane > 0)]).ok()?;
        let (w, h) = self.plane_size(plane);
        let sample = if self.bit_depth() > 8 { 2 } else { 1 };
        let length = stride * (h as usize).saturating_sub(1) + w as usize * sample;
        // SAFETY: rav1d's plane holds `h` rows `stride` bytes apart, each with at least `w`
        // samples, and lives while this picture holds its reference.
        Some((
            unsafe { core::slice::from_raw_parts(data.as_ptr().cast::<u8>(), length) },
            stride,
        ))
    }

    /// The header of the frame the picture is (for a shown existing frame, the header it
    /// was decoded with).
    pub fn frame_header(&self) -> Option<&Dav1dFrameHeader> {
        // SAFETY: rav1d keeps the header alive while the picture holds its reference.
        self.picture
            .frame_hdr
            .map(|header| unsafe { header.as_ref() })
    }

    /// The sequence header the picture was decoded with.
    pub fn sequence_header(&self) -> Option<&Dav1dSequenceHeader> {
        // SAFETY: rav1d keeps the header alive while the picture holds its reference.
        self.picture
            .seq_hdr
            .map(|header| unsafe { header.as_ref() })
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture came from dav1d_get_picture and is released once.
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut self.picture))) };
    }
}
