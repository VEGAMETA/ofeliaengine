//! A video's pictures as a texture (ADR 0100): the pass that converts a picture's Y'CbCr
//! planes into the video's RGBA texture, a compute pass of the frame graph on the
//! graphics queue.
//!
//! - [`VideoTexture`]: what materials, the UI and viewports sample. RGBA8, R'G'B' written
//!   through a UNORM view and sampled through an sRGB one, so filtering happens in linear
//!   light; in the bindless table at [`VideoTexture::sampled_index`].
//! - [`Conversion`]: a stream's colour maths (matrix, range, bit depth, chroma siting)
//!   from its colour description; [`Conversion::rgb`] does it on the CPU, the tests'
//!   reference.
//! - [`PlaneSource`]: the planes one pass reads. A GPU decoder's pictures are read where
//!   they were decoded ([`DecoderPlanes`]); the software decoder's are uploaded first
//!   ([`PlaneUpload`]).
//! - [`Converter`]: the pass ([`Converter::add_pass`]).
//!
//! The pass is a fence of the frame graph: shaders read the texture through its bindless
//! index without declaring it, so nothing declared after the pass runs before it, on
//! either queue. A GPU decoder's picture must be decoded before the pass reads it:
//! [`wait_for_decode`] holds the graphics queue back until it is, submitted before the
//! frame graph's first submission. The texture is read on the graphics queue only, as
//! material textures are (the async compute queue's passes sample none).
//!
//! A stream whose colour description names an HDR transfer (PQ, HLG) or BT.2020's
//! primaries is shown as SDR with BT.709's: its matrix is used, its gamut and curve are
//! not converted yet (ADR 0100).

use core::ffi::CStr;

use engine_render::compute::{ComputeShader, Dispatch};
use engine_render::graph::FrameGraph;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError};

use crate::av1::sequence::ColorConfig;
use crate::gpu::{GpuDecoder, GpuError, GpuPicture};

/// The conversion shader, built from `shaders/convert.slang` (`tests/shaders.rs` checks
/// that it is up to date).
const CONVERT_SPV: &[u8] = include_bytes!("../shaders/convert.convertPicture.spv");
const ENTRY: &CStr = c"convertPicture";

/// The texture's view formats: written as UNORM, sampled as sRGB.
const VIEW_FORMATS: [vk::Format; 2] = [vk::Format::R8G8B8A8_UNORM, vk::Format::R8G8B8A8_SRGB];

const COLOR: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

/// Kr and Kb of BT.709 and BT.601, the matrices players fall back to.
const BT709: (f32, f32) = (0.2126, 0.0722);
const BT601: (f32, f32) = (0.299, 0.114);

/// How a stream's code values become R'G'B' in [0, 1] (ITU-T H.273, whose code points
/// AV1's colour description uses), and where its chroma samples sit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conversion {
    /// R', G' and B' from the code values `(Y, Cb, Cr, 1)`: the rows of an affine map.
    pub rows: [[f32; 4]; 3],
    /// Chroma samples per luma sample, across and down (0.5 subsampled, 0 without chroma).
    pub chroma_step: [f32; 2],
    /// The chroma position of luma sample 0, in chroma samples (the siting).
    pub chroma_bias: [f32; 2],
}

impl Conversion {
    /// The conversion of a stream with colour description `color` and pictures of `width
    /// × height`. A matrix the stream does not name (unspecified, reserved, or one not done
    /// here) is guessed from the size as players guess it: BT.709 from 1280 wide or 577
    /// high, BT.601 below.
    pub fn new(color: &ColorConfig, width: u32, height: u32) -> Self {
        let depth = u32::from(color.bit_depth.clamp(8, 16));
        let (luma, chroma) = normalization(depth, color.color_range);
        let guess = if width >= 1280 || height > 576 {
            BT709
        } else {
            BT601
        };
        // The matrix from normalized (E'Y, E'Cb, E'Cr), and how its second and third
        // columns are normalized.
        let (matrix, others) = match color.matrix_coefficients {
            // GBR: the planes hold G, B and R, each normalized as luma.
            0 => ([[0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], luma),
            1 => (coefficients(BT709), chroma),
            4 => (coefficients((0.30, 0.11)), chroma),
            5 | 6 => (coefficients(BT601), chroma),
            7 => (coefficients((0.212, 0.087)), chroma),
            // YCgCo: Cg in the Cb plane, Co in the Cr plane.
            8 => (
                [[1.0, -1.0, 1.0], [1.0, 1.0, 0.0], [1.0, -1.0, -1.0]],
                chroma,
            ),
            // BT.2020's constant luminance form (10) taken as its usual one.
            9 | 10 => (coefficients((0.2627, 0.0593)), chroma),
            _ => (coefficients(guess), chroma),
        };
        let scale = [luma, others, others];
        let rows = matrix.map(|m| {
            let mut row = [0.0; 4];
            for j in 0..3 {
                row[j] = m[j] * scale[j].0;
                row[3] += m[j] * scale[j].1;
            }
            row
        });
        let (chroma_step, chroma_bias) = if color.mono_chrome {
            ([0.0; 2], [0.0; 2])
        } else {
            let step = |subsampled: bool| if subsampled { 0.5 } else { 1.0 };
            // 4:2:0's chroma sits on luma sample 0 across, and halfway to the next one
            // down (AV1's "vertical" position, and the usual one when unknown) unless it
            // is "colocated" (2).
            let down = if color.subsampling_x
                && color.subsampling_y
                && color.chroma_sample_position != 2
            {
                -0.25
            } else {
                0.0
            };
            (
                [step(color.subsampling_x), step(color.subsampling_y)],
                [0.0, down],
            )
        };
        Self {
            rows,
            chroma_step,
            chroma_bias,
        }
    }

    /// The R'G'B' bytes of code values `ycc`, rounded as the pass rounds them.
    pub fn rgb(&self, ycc: [f32; 3]) -> [u8; 3] {
        self.rows.map(|r| {
            let v = r[0] * ycc[0] + r[1] * ycc[1] + r[2] * ycc[2] + r[3];
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        })
    }
}

/// The R'G'B' matrix of a Kr, Kb pair: R' = Y' + 2 (1 − Kr) Cr, B' = Y' + 2 (1 − Kb) Cb,
/// and G' from the luma equation.
fn coefficients((kr, kb): (f32, f32)) -> [[f32; 3]; 3] {
    let kg = 1.0 - kr - kb;
    [
        [1.0, 0.0, 2.0 * (1.0 - kr)],
        [
            1.0,
            -2.0 * kb * (1.0 - kb) / kg,
            -2.0 * kr * (1.0 - kr) / kg,
        ],
        [1.0, 2.0 * (1.0 - kb), 0.0],
    ]
}

/// `(scale, offset)` pairs taking luma and chroma code values of `depth` bits to E'Y in
/// [0, 1] and E'Cb, E'Cr in [−0.5, 0.5] (H.273's equations for full and narrow range).
fn normalization(depth: u32, full: bool) -> ((f32, f32), (f32, f32)) {
    if full {
        let max = ((1u32 << depth) - 1) as f32;
        let mid = (1u32 << (depth - 1)) as f32;
        ((1.0 / max, 0.0), (1.0 / max, -mid / max))
    } else {
        let k = (1u32 << (depth - 8)) as f32;
        (
            (1.0 / (219.0 * k), -16.0 / 219.0),
            (1.0 / (224.0 * k), -128.0 / 224.0),
        )
    }
}

/// The planes one pass reads: a luma plane and an interleaved chroma plane (Cb, Cr) in
/// the bindless table, and how their samples hold the code values.
#[derive(Debug, Clone, Copy)]
pub struct PlaneSource {
    luma: u32,
    chroma: u32,
    size: [u32; 2],
    chroma_size: [u32; 2],
    /// A raw sample's largest value (255, or 65535 for 16-bit words).
    raw_scale: f32,
    /// The low bits a raw sample drops (6 in P010).
    shift: u32,
    upload: Option<Upload>,
}

impl PlaneSource {
    /// The picture's size in luma samples.
    pub fn size(&self) -> [u32; 2] {
        self.size
    }
}

/// Copies from staging memory into the plane images, recorded before the conversion.
#[derive(Debug, Clone, Copy)]
struct Upload {
    buffer: vk::Buffer,
    images: [vk::Image; 2],
    regions: [vk::BufferImageCopy; 2],
}

/// A GPU decoder's output images' planes in the bindless table: its pictures are read
/// where they were decoded.
#[derive(Debug)]
pub struct DecoderPlanes {
    /// Per output image, its luma and chroma planes' indices in `g_textures`.
    indices: Vec<[u32; 2]>,
    /// P010 (16-bit words, samples in the high ten bits) rather than NV12.
    wide: bool,
}

impl DecoderPlanes {
    /// Puts `decoder`'s plane views in `bindless`.
    ///
    /// # Errors
    ///
    /// The device cannot view the planes alone ([`GpuDecoder::plane_views`]), or the table
    /// is full.
    pub fn new(decoder: &GpuDecoder, bindless: &Bindless) -> Result<Self, GpuError> {
        let mut planes = Self {
            indices: Vec::with_capacity(decoder.outputs() as usize),
            wide: decoder.caps().format == vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
        };
        for output in 0..decoder.outputs() {
            let Some([luma, chroma]) = decoder.plane_views(output) else {
                planes.release(bindless, 0);
                return Err(GpuError::Unsupported(
                    "sampling the decoded planes (no mutable format)",
                ));
            };
            let Some(luma) = bindless.add_sampled_image(luma) else {
                planes.release(bindless, 0);
                return Err(GpuError::Rhi(bindless_full()));
            };
            let Some(chroma) = bindless.add_sampled_image(chroma) else {
                bindless.remove_sampled_image(luma, 0);
                planes.release(bindless, 0);
                return Err(GpuError::Rhi(bindless_full()));
            };
            planes.indices.push([luma, chroma]);
        }
        Ok(planes)
    }

    /// The planes of `picture`, one of the decoder's.
    pub fn source(&self, picture: &GpuPicture) -> Option<PlaneSource> {
        let [luma, chroma] = *self.indices.get(picture.output() as usize)?;
        let (raw_scale, shift) = if self.wide { (65535.0, 6) } else { (255.0, 0) };
        Some(PlaneSource {
            luma,
            chroma,
            size: [picture.width, picture.height],
            chroma_size: [picture.width.div_ceil(2), picture.height.div_ceil(2)],
            raw_scale,
            shift,
            upload: None,
        })
    }

    /// Gives the indices back to `bindless`, reused once frame `last_frame` has finished.
    pub fn release(self, bindless: &Bindless, last_frame: u64) {
        for index in self.indices.into_iter().flatten() {
            bindless.remove_sampled_image(index, last_frame);
        }
    }
}

/// A plane in memory: its rows of samples and the bytes from one row to the next.
pub type Rows<'a> = (&'a [u8], usize);

/// A picture's planes in memory (the software decoder's): rows of samples, bytes at 8
/// bits, little-endian 16-bit words with the code in their low bits above.
#[derive(Debug, Clone, Copy)]
pub struct Planes<'a> {
    /// The picture's size in luma samples.
    pub size: [u32; 2],
    /// Bits per sample, 8 to 16.
    pub bit_depth: u8,
    /// The luma plane.
    pub luma: Rows<'a>,
    /// The Cb and Cr planes, as the luma plane, and their size in samples; `None` for a
    /// picture of luma only.
    pub chroma: Option<([Rows<'a>; 2], [u32; 2])>,
}

#[cfg(feature = "software")]
impl<'a> From<&'a crate::software::Picture> for Planes<'a> {
    fn from(picture: &'a crate::software::Picture) -> Self {
        let chroma = match (picture.plane(1), picture.plane(2)) {
            (Some(cb), Some(cr)) => {
                let (w, h) = picture.plane_size(1);
                Some(([cb, cr], [w, h]))
            }
            _ => None,
        };
        Self {
            size: [picture.width(), picture.height()],
            bit_depth: picture.bit_depth() as u8,
            luma: picture.plane(0).unwrap_or((&[], 0)),
            chroma,
        }
    }
}

/// An image of one plane of the software decoder's pictures.
struct PlaneImage {
    image: vk::Image,
    allocation: Option<Allocation>,
    view: vk::ImageView,
    index: u32,
    size: [u32; 2],
}

/// The software decoder's pictures on the GPU: each is copied into its frame slot's
/// staging memory, then by the pass into two plane images (luma; Cb and Cr interleaved).
pub struct PlaneUpload {
    images: Vec<PlaneImage>,
    staging: Vec<(vk::Buffer, Option<Allocation>)>,
    /// Bytes per sample: 1, or 2 above 8 bits.
    sample: usize,
    /// The chroma plane's place in the staging memory.
    chroma_offset: u64,
    /// The stream has chroma.
    has_chroma: bool,
}

impl core::fmt::Debug for PlaneUpload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PlaneUpload")
            .field("sample", &self.sample)
            .field("slots", &self.staging.len())
            .finish_non_exhaustive()
    }
}

impl PlaneUpload {
    /// Plane images for pictures of a stream with colour description `color` up to `max`
    /// luma samples (its largest frame, rounded up to whole 64-sample superblocks: some
    /// encoders code frames past the sequence header's maximum), and staging memory for
    /// `in_flight` frame slots.
    ///
    /// # Errors
    ///
    /// Out of memory, or a full bindless table.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        color: &ColorConfig,
        max: [u32; 2],
        in_flight: usize,
    ) -> Result<Self, RhiError> {
        let max = max.map(|m| m.max(1).next_multiple_of(64));
        let wide = color.bit_depth > 8;
        let sample = if wide { 2 } else { 1 };
        let chroma_max = if color.mono_chrome {
            [1, 1]
        } else {
            [
                if color.subsampling_x {
                    max[0].div_ceil(2)
                } else {
                    max[0]
                },
                if color.subsampling_y {
                    max[1].div_ceil(2)
                } else {
                    max[1]
                },
            ]
        };
        let formats = if wide {
            [vk::Format::R16_UNORM, vk::Format::R16G16_UNORM]
        } else {
            [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM]
        };
        let luma_bytes = u64::from(max[0]) * u64::from(max[1]) * sample as u64;
        let chroma_offset = luma_bytes.next_multiple_of(16);
        let chroma_bytes = u64::from(chroma_max[0]) * u64::from(chroma_max[1]) * 2 * sample as u64;
        let mut upload = Self {
            images: Vec::with_capacity(2),
            staging: Vec::with_capacity(in_flight),
            sample,
            chroma_offset,
            has_chroma: !color.mono_chrome,
        };
        let made = (|| {
            for (format, size) in formats.into_iter().zip([max, chroma_max]) {
                let image = plane_image(device, allocator, bindless, format, size)?;
                upload.images.push(image);
            }
            for _ in 0..in_flight.max(1) {
                let (buffer, allocation) = allocator.create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(chroma_offset + chroma_bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                    MemoryLocation::Upload,
                )?;
                upload.staging.push((buffer, Some(allocation)));
            }
            Ok(())
        })();
        match made {
            Ok(()) => Ok(upload),
            Err(e) => {
                // SAFETY: nothing was recorded with them.
                unsafe { upload.destroy(device, allocator, bindless, 0) };
                Err(e)
            }
        }
    }

    /// Copies `planes` into frame slot `slot`'s staging memory (its previous frame has
    /// finished, `engine_rhi::Frames::begin`) and returns them as the pass's source; the
    /// pass copies them into the plane images.
    ///
    /// # Errors
    ///
    /// A picture larger than the images, of another sample size, or with chroma where the
    /// stream has none (or none where it has).
    pub fn stage(&mut self, slot: usize, planes: &Planes<'_>) -> Result<PlaneSource, GpuError> {
        let sample = if planes.bit_depth > 8 { 2 } else { 1 };
        let [w, h] = planes.size;
        let chroma_size = planes.chroma.map_or([1, 1], |(_, size)| size);
        if sample != self.sample
            || planes.chroma.is_some() != self.has_chroma
            || w > self.images[0].size[0]
            || h > self.images[0].size[1]
            || chroma_size[0] > self.images[1].size[0]
            || chroma_size[1] > self.images[1].size[1]
        {
            return Err(GpuError::Unsupported(
                "a picture unlike the stream's planes",
            ));
        }
        let index = slot % self.staging.len();
        let (buffer, allocation) = &mut self.staging[index];
        let Some(mapped) = allocation.as_mut().and_then(Allocation::mapped_slice_mut) else {
            return Err(GpuError::Unsupported("staging memory the CPU cannot write"));
        };
        let row = w as usize * sample;
        let (luma, stride) = planes.luma;
        for y in 0..h as usize {
            let from = y * stride;
            mapped[y * row..(y + 1) * row].copy_from_slice(&luma[from..from + row]);
        }
        let offset = self.chroma_offset as usize;
        match planes.chroma {
            Some(([(cb, cb_stride), (cr, cr_stride)], [cw, ch])) => {
                let row = cw as usize * sample;
                for y in 0..ch as usize {
                    let cb = &cb[y * cb_stride..y * cb_stride + row];
                    let cr = &cr[y * cr_stride..y * cr_stride + row];
                    let to = &mut mapped[offset + y * 2 * row..offset + (y + 1) * 2 * row];
                    if sample == 1 {
                        for ((pair, &b), &r) in to.chunks_exact_mut(2).zip(cb).zip(cr) {
                            pair[0] = b;
                            pair[1] = r;
                        }
                    } else {
                        for ((pair, b), r) in to
                            .chunks_exact_mut(4)
                            .zip(cb.chunks_exact(2))
                            .zip(cr.chunks_exact(2))
                        {
                            pair[..2].copy_from_slice(b);
                            pair[2..].copy_from_slice(r);
                        }
                    }
                }
            }
            None => {
                // No chroma: one neutral sample, read wherever the pass looks.
                let mid = 1u16 << (planes.bit_depth.clamp(8, 16) - 1);
                if sample == 1 {
                    mapped[offset..offset + 2].fill(mid as u8);
                } else {
                    for pair in mapped[offset..offset + 4].chunks_exact_mut(2) {
                        pair.copy_from_slice(&mid.to_le_bytes());
                    }
                }
            }
        }
        let region = |offset: u64, [width, height]: [u32; 2]| vk::BufferImageCopy {
            buffer_offset: offset,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: vk::Offset3D::default(),
            image_extent: vk::Extent3D {
                width,
                height,
                depth: 1,
            },
        };
        Ok(PlaneSource {
            luma: self.images[0].index,
            chroma: self.images[1].index,
            size: planes.size,
            chroma_size,
            raw_scale: if sample == 1 { 255.0 } else { 65535.0 },
            shift: 0,
            upload: Some(Upload {
                buffer: *buffer,
                images: [self.images[0].image, self.images[1].image],
                regions: [
                    region(0, planes.size),
                    region(self.chroma_offset, chroma_size),
                ],
            }),
        })
    }

    /// Destroys the images and the staging memory; their bindless indices are reused once
    /// frame `last_frame` has finished.
    ///
    /// # Safety
    ///
    /// `device`, `allocator` and `bindless` are those it was made with; the GPU no longer
    /// uses it.
    pub unsafe fn destroy(
        mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        last_frame: u64,
    ) {
        for mut image in self.images.drain(..) {
            bindless.remove_sampled_image(image.index, last_frame);
            // SAFETY: the GPU no longer uses them (the caller's contract).
            unsafe {
                device.raw().destroy_image_view(image.view, None);
                if let Some(allocation) = image.allocation.take() {
                    allocator.destroy_image(image.image, allocation);
                }
            }
        }
        for (buffer, allocation) in &mut self.staging {
            if let Some(allocation) = allocation.take() {
                // SAFETY: as above.
                unsafe { allocator.destroy_buffer(*buffer, allocation) };
            }
        }
    }
}

/// A plane image with its sampled view in `bindless`.
fn plane_image(
    device: &Device,
    allocator: &mut Allocator,
    bindless: &Bindless,
    format: vk::Format,
    size: [u32; 2],
) -> Result<PlaneImage, RhiError> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: size[0],
            height: size[1],
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let (image, allocation) = allocator.create_image(&info, MemoryLocation::Device)?;
    let raw = device.raw();
    // SAFETY: a view of the image just made.
    let view = unsafe {
        raw.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(COLOR),
            None,
        )
    };
    let view = match view {
        Ok(view) => view,
        Err(result) => {
            // SAFETY: never used.
            unsafe { allocator.destroy_image(image, allocation) };
            return Err(RhiError::vk("vkCreateImageView")(result));
        }
    };
    let Some(index) = bindless.add_sampled_image(view) else {
        // SAFETY: never used.
        unsafe {
            raw.destroy_image_view(view, None);
            allocator.destroy_image(image, allocation);
        }
        return Err(bindless_full());
    };
    Ok(PlaneImage {
        image,
        allocation: Some(allocation),
        view,
        index,
        size,
    })
}

fn bindless_full() -> RhiError {
    RhiError::vk("bindless image slots")(vk::Result::ERROR_OUT_OF_POOL_MEMORY)
}

/// A video's picture as materials, the UI and viewports read it (see the module).
pub struct VideoTexture {
    image: vk::Image,
    allocation: Option<Allocation>,
    /// UNORM: the pass writes R'G'B' through it.
    storage_view: vk::ImageView,
    /// sRGB: shaders sample linear R, G and B through it.
    sampled_view: vk::ImageView,
    sampled: Option<u32>,
    storage: Option<u32>,
    width: u32,
    height: u32,
    /// Not written yet (still in its initial layout).
    fresh: bool,
}

impl core::fmt::Debug for VideoTexture {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VideoTexture")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("sampled", &self.sampled)
            .finish_non_exhaustive()
    }
}

impl VideoTexture {
    /// A `width × height` texture in `bindless`. Its contents are undefined until a pass
    /// writes it ([`Self::is_written`]): nothing should sample it before.
    ///
    /// # Errors
    ///
    /// Out of memory, or a full bindless table.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        width: u32,
        height: u32,
    ) -> Result<Self, RhiError> {
        let mut list = vk::ImageFormatListCreateInfo::default().view_formats(&VIEW_FORMATS);
        let info = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .image_type(vk::ImageType::TYPE_2D)
            .format(VIEW_FORMATS[0])
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::STORAGE
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut list);
        let (image, allocation) = allocator.create_image(&info, MemoryLocation::Device)?;
        let mut texture = Self {
            image,
            allocation: Some(allocation),
            storage_view: vk::ImageView::null(),
            sampled_view: vk::ImageView::null(),
            sampled: None,
            storage: None,
            width,
            height,
            fresh: true,
        };
        match texture.finish(device, bindless) {
            Ok(()) => Ok(texture),
            Err(e) => {
                // SAFETY: nothing was recorded with it.
                unsafe { texture.destroy(device, allocator, bindless, 0) };
                Err(e)
            }
        }
    }

    /// The views and their bindless indices.
    fn finish(&mut self, device: &Device, bindless: &Bindless) -> Result<(), RhiError> {
        let raw = device.raw();
        let view = |format: vk::Format, usage: vk::ImageUsageFlags| {
            let mut usage = vk::ImageViewUsageCreateInfo::default().usage(usage);
            // SAFETY: a view of this texture's image, in one of its listed formats.
            unsafe {
                raw.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(self.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .subresource_range(COLOR)
                        .push_next(&mut usage),
                    None,
                )
            }
            .map_err(RhiError::vk("vkCreateImageView"))
        };
        self.storage_view = view(VIEW_FORMATS[0], vk::ImageUsageFlags::STORAGE)?;
        self.sampled_view = view(VIEW_FORMATS[1], vk::ImageUsageFlags::SAMPLED)?;
        self.storage = Some(
            bindless
                .add_storage_image(self.storage_view)
                .ok_or_else(bindless_full)?,
        );
        self.sampled = Some(
            bindless
                .add_sampled_image(self.sampled_view)
                .ok_or_else(bindless_full)?,
        );
        Ok(())
    }

    /// Its index in the bindless table's sampled images (`g_textures`): linear R, G and B
    /// through the sRGB view, alpha 1.
    pub fn sampled_index(&self) -> u32 {
        self.sampled.unwrap_or(0)
    }

    /// Its size.
    pub fn size(&self) -> [u32; 2] {
        [self.width, self.height]
    }

    /// The image (for copies, in `READ_ONLY_OPTIMAL` once written).
    pub fn image(&self) -> vk::Image {
        self.image
    }

    /// A pass has written it (it may be sampled).
    pub fn is_written(&self) -> bool {
        !self.fresh
    }

    /// Destroys it; its bindless indices are reused once frame `last_frame` has finished.
    ///
    /// # Safety
    ///
    /// `device`, `allocator` and `bindless` are those it was made with; the GPU no longer
    /// uses it.
    pub unsafe fn destroy(
        mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        last_frame: u64,
    ) {
        if let Some(index) = self.sampled.take() {
            bindless.remove_sampled_image(index, last_frame);
        }
        if let Some(index) = self.storage.take() {
            bindless.remove_storage_image(index, last_frame);
        }
        let raw = device.raw();
        // SAFETY: the GPU no longer uses them (the caller's contract).
        unsafe {
            for view in [self.storage_view, self.sampled_view] {
                if view != vk::ImageView::null() {
                    raw.destroy_image_view(view, None);
                }
            }
            if let Some(allocation) = self.allocation.take() {
                allocator.destroy_image(self.image, allocation);
            }
        }
    }
}

/// The conversion shader's push constants (`ConvertPush` in `convert.slang`).
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct ConvertPush {
    to_red: [f32; 4],
    to_green: [f32; 4],
    to_blue: [f32; 4],
    luma: u32,
    chroma: u32,
    target: u32,
    shift: u32,
    source_size: [u32; 2],
    target_size: [u32; 2],
    source_step: [f32; 2],
    chroma_step: [f32; 2],
    chroma_bias: [f32; 2],
    chroma_size: [u32; 2],
    raw_scale: f32,
    _pad: [u32; 3],
}

/// The conversion pass (one for a device's videos).
pub struct Converter {
    shader: ComputeShader<ConvertPush>,
}

impl core::fmt::Debug for Converter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Converter").finish_non_exhaustive()
    }
}

impl Converter {
    /// Creates the pass's pipeline.
    ///
    /// # Errors
    ///
    /// The pipeline could not be made.
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        Ok(Self {
            shader: ComputeShader::new(device, bindless, CONVERT_SPV, ENTRY)?,
        })
    }

    /// Adds the pass that converts `source` into `texture` with `conversion` to the frame
    /// graph, scaled to the texture's size when the picture's differs. The pass is a
    /// fence: what is declared after it runs after it. A GPU decoder's picture must be
    /// decoded before the graph's first submission ([`wait_for_decode`]); an upload's
    /// staging memory must stay untouched until the frame finishes.
    pub fn add_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        texture: &mut VideoTexture,
        source: &PlaneSource,
        conversion: &Conversion,
    ) {
        let [width, height] = [texture.width, texture.height];
        let push = ConvertPush {
            to_red: conversion.rows[0],
            to_green: conversion.rows[1],
            to_blue: conversion.rows[2],
            luma: source.luma,
            chroma: source.chroma,
            target: texture.storage.unwrap_or(0),
            shift: source.shift,
            source_size: source.size,
            target_size: [width, height],
            source_step: [
                source.size[0] as f32 / width as f32,
                source.size[1] as f32 / height as f32,
            ],
            chroma_step: conversion.chroma_step,
            chroma_bias: conversion.chroma_bias,
            chroma_size: source.chroma_size,
            raw_scale: source.raw_scale,
            _pad: [0; 3],
        };
        let image = texture.image;
        let fresh = core::mem::replace(&mut texture.fresh, false);
        let upload = source.upload;
        let shader = &self.shader;
        fg.add_compute("video conversion", bindless)
            .fence()
            .side_effects()
            .record(move |rec| {
                let (device, cmd) = (rec.context().device, rec.context().cmd);
                // SAFETY: recording outside a rendering scope (a compute pass); the
                // images and the staging buffer live until the frame finishes (their
                // owners' contracts).
                unsafe { before(device, cmd, image, fresh, upload.as_ref()) };
                rec.dispatch(shader, Dispatch::Threads([width, height, 1]), &push);
                let readable = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::READ_ONLY_OPTIMAL)
                    .image(image)
                    .subresource_range(COLOR);
                // SAFETY: as above.
                unsafe {
                    device.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[readable]),
                    );
                }
            });
    }
}

/// The barriers (and an upload's copies) before the conversion's dispatch: the texture
/// becomes writable after every earlier read of it; uploaded planes are copied in.
///
/// # Safety
///
/// `cmd` is recording outside a rendering scope; the images and the buffer are alive.
unsafe fn before(
    device: &engine_rhi::ash::Device,
    cmd: vk::CommandBuffer,
    texture: vk::Image,
    fresh: bool,
    upload: Option<&Upload>,
) {
    let (stage, layout) = if fresh {
        (vk::PipelineStageFlags2::NONE, vk::ImageLayout::UNDEFINED)
    } else {
        // Read by any shader of the frames before.
        (
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::ImageLayout::READ_ONLY_OPTIMAL,
        )
    };
    let writable = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(stage)
        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        .old_layout(layout)
        .new_layout(vk::ImageLayout::GENERAL)
        .image(texture)
        .subresource_range(COLOR);
    let Some(upload) = upload else {
        // SAFETY: the caller's contract.
        unsafe {
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[writable]),
            );
        }
        return;
    };
    // The planes' previous contents were read by the previous conversion only.
    let [luma, chroma] = upload.images.map(|image| {
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(image)
            .subresource_range(COLOR)
    });
    let [luma_read, chroma_read] = upload.images.map(|image| {
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::READ_ONLY_OPTIMAL)
            .image(image)
            .subresource_range(COLOR)
    });
    // SAFETY: the caller's contract; the regions lie inside the images and the buffer
    // (`PlaneUpload::stage`).
    unsafe {
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[luma, chroma, writable]),
        );
        for (image, region) in upload.images.iter().zip(&upload.regions) {
            device.cmd_copy_buffer_to_image(
                cmd,
                upload.buffer,
                *image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                core::slice::from_ref(region),
            );
        }
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[luma_read, chroma_read]),
        );
    }
}

/// Holds the graphics queue's later work back until `semaphore` (a GPU decoder's) reaches
/// `value`, where its picture is decoded: an empty batch, to submit before the frame
/// graph's first submission. Costs the GPU nothing when the value is already reached (the
/// player converts only pictures it has seen decoded).
///
/// # Errors
///
/// A failed submission (a lost device).
pub fn wait_for_decode(
    device: &Device,
    semaphore: vk::Semaphore,
    value: u64,
) -> Result<(), RhiError> {
    let waits = [vk::SemaphoreSubmitInfo::default()
        .semaphore(semaphore)
        .value(value)
        .stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)];
    // SAFETY: the semaphore is this device's; the graphics queue is submitted to from
    // the caller's thread only, as every submission of the frame is.
    unsafe {
        device.raw().queue_submit2(
            device.graphics.raw,
            &[vk::SubmitInfo2::default().wait_semaphore_infos(&waits)],
            vk::Fence::null(),
        )
    }
    .map_err(RhiError::vk("vkQueueSubmit2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color(matrix: u8, full: bool, depth: u8) -> ColorConfig {
        ColorConfig {
            bit_depth: depth,
            high_bitdepth: depth > 8,
            twelve_bit: depth == 12,
            color_description_present_flag: true,
            color_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: matrix,
            color_range: full,
            subsampling_x: true,
            subsampling_y: true,
            ..ColorConfig::default()
        }
    }

    fn near(ours: [u8; 3], theirs: [u8; 3]) -> bool {
        ours.iter().zip(theirs).all(|(&a, b)| a.abs_diff(b) <= 1)
    }

    #[test]
    fn white_black_and_red_in_studio_range() {
        let bt709 = Conversion::new(&color(1, false, 8), 1920, 1080);
        assert_eq!(bt709.rgb([235.0, 128.0, 128.0]), [255, 255, 255]);
        assert_eq!(bt709.rgb([16.0, 128.0, 128.0]), [0, 0, 0]);
        // BT.709's red, quantized: (63, 102, 240).
        assert!(near(bt709.rgb([63.0, 102.0, 240.0]), [255, 0, 0]));
        let bt601 = Conversion::new(&color(6, false, 8), 640, 480);
        assert!(near(bt601.rgb([81.0, 90.0, 240.0]), [255, 0, 0]));
        // Ten bits: the same at four times the codes.
        let ten = Conversion::new(&color(1, false, 10), 1920, 1080);
        assert_eq!(ten.rgb([940.0, 512.0, 512.0]), [255, 255, 255]);
        assert_eq!(ten.rgb([64.0, 512.0, 512.0]), [0, 0, 0]);
        assert!(near(ten.rgb([250.0, 409.0, 960.0]), [255, 0, 0]));
    }

    #[test]
    fn full_range_and_identity() {
        let full = Conversion::new(&color(1, true, 8), 1920, 1080);
        assert_eq!(full.rgb([255.0, 128.0, 128.0]), [255, 255, 255]);
        assert_eq!(full.rgb([0.0, 128.0, 128.0]), [0, 0, 0]);
        // GBR: the planes hold G, B and R.
        let gbr = Conversion::new(&color(0, true, 8), 64, 64);
        assert_eq!(gbr.rgb([10.0, 20.0, 30.0]), [30, 10, 20]);
    }

    #[test]
    fn unspecified_matrices_follow_the_size() {
        let hd = Conversion::new(&color(2, false, 8), 1280, 720);
        let sd = Conversion::new(&color(2, false, 8), 640, 360);
        assert_eq!(hd.rows, Conversion::new(&color(1, false, 8), 1, 1).rows);
        assert_eq!(sd.rows, Conversion::new(&color(6, false, 8), 1, 1).rows);
    }

    #[test]
    fn chroma_siting() {
        let mut c = color(1, false, 8);
        let left = Conversion::new(&c, 64, 64);
        assert_eq!(
            (left.chroma_step, left.chroma_bias),
            ([0.5, 0.5], [0.0, -0.25])
        );
        c.chroma_sample_position = 2;
        let colocated = Conversion::new(&c, 64, 64);
        assert_eq!(colocated.chroma_bias, [0.0, 0.0]);
        c.subsampling_y = false;
        let across = Conversion::new(&c, 64, 64);
        assert_eq!(
            (across.chroma_step, across.chroma_bias),
            ([0.5, 1.0], [0.0, 0.0])
        );
        c.mono_chrome = true;
        assert_eq!(Conversion::new(&c, 64, 64).chroma_step, [0.0, 0.0]);
    }
}
