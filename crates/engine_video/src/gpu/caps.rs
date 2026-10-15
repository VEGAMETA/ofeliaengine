//! What the device's video decoder does with AV1: the video profile, its capabilities
//! and the picture formats (Vulkan's `vkGetPhysicalDeviceVideoCapabilitiesKHR` and
//! `vkGetPhysicalDeviceVideoFormatPropertiesKHR`).

use engine_rhi::Device;
use engine_rhi::ash::khr::video_queue;
use engine_rhi::ash::vk;

use super::GpuError;
use crate::av1::SequenceHeader;

/// `seq_level_idx` 31: no level, the stream's own limits (rav1e writes it).
const LEVEL_MAX_PARAMETERS: u8 = 31;

/// An AV1 decode profile (Vulkan's `VkVideoProfileInfoKHR` with its AV1 part), kept in a
/// box so the chain's pointer stays valid wherever the box goes.
pub(crate) struct Profile {
    av1: vk::VideoDecodeAV1ProfileInfoKHR<'static>,
    info: vk::VideoProfileInfoKHR<'static>,
}

// SAFETY: the raw pointer in `info` points into the same box, which is never mutated
// after construction.
unsafe impl Send for Profile {}
// SAFETY: as above, and the structures are only read.
unsafe impl Sync for Profile {}

impl Profile {
    /// AV1's main profile (8 or 10 bits, 4:2:0), with film grain applied by the decoder
    /// when `film_grain`.
    pub(crate) fn new(bit_depth: u8, film_grain: bool) -> Box<Self> {
        let depth = if bit_depth > 8 {
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        } else {
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8
        };
        let mut profile = Box::new(Self {
            av1: vk::VideoDecodeAV1ProfileInfoKHR::default()
                .std_profile(vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN)
                .film_grain_support(film_grain),
            info: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_AV1)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(depth)
                .chroma_bit_depth(depth),
        });
        profile.info.p_next = (&raw const profile.av1).cast();
        profile
    }

    /// The profile, for the calls and structures that take one.
    pub(crate) fn info(&self) -> &vk::VideoProfileInfoKHR<'static> {
        &self.info
    }
}

/// What the device's AV1 decoder can do for one profile (a bit depth, film grain or not).
#[derive(Debug, Clone, Copy)]
pub struct GpuCaps {
    /// The decoded picture may be written into its reference picture
    /// (`DPB_AND_OUTPUT_COINCIDE`; NVIDIA).
    pub coincide: bool,
    /// The decoded picture may be written into an image of its own
    /// (`DPB_AND_OUTPUT_DISTINCT`; AMD).
    pub distinct: bool,
    /// The highest AV1 level (`seq_level_idx`: 23 is 7.3).
    pub max_level: u8,
    /// The smallest and largest coded picture.
    pub min_extent: vk::Extent2D,
    /// The largest coded picture.
    pub max_extent: vk::Extent2D,
    /// Reference picture slots a session may have.
    pub max_dpb_slots: u32,
    /// References a frame may read.
    pub max_active_references: u32,
    /// Pictures are read and written in blocks of this size.
    pub granularity: vk::Extent2D,
    /// The bitstream's offset in its buffer is a multiple of this.
    pub bitstream_offset_alignment: u64,
    /// The bitstream's range in its buffer is a multiple of this.
    pub bitstream_size_alignment: u64,
    /// The Video Std header version the driver implements.
    pub std_header_version: vk::ExtensionProperties,
    /// The pictures' format: NV12 (8 bits) or P010 (10 bits).
    pub format: vk::Format,
    /// What reference picture images may be created with (flags, usage).
    pub dpb_flags: vk::ImageCreateFlags,
    /// The usage reference picture images support (with `VIDEO_DECODE_DPB`).
    pub dpb_usage: vk::ImageUsageFlags,
    /// What output images may be created with.
    pub output_flags: vk::ImageCreateFlags,
    /// The usage output images support (with `VIDEO_DECODE_DST`).
    pub output_usage: vk::ImageUsageFlags,
}

impl GpuCaps {
    /// Queries the device's decoder for AV1's main profile at `bit_depth`, with film grain
    /// applied by it when `film_grain`.
    ///
    /// # Errors
    ///
    /// No decode queue on the device, or a profile the decoder does not take.
    pub fn query(device: &Device, bit_depth: u8, film_grain: bool) -> Result<Self, GpuError> {
        if device.video_decode.is_none() {
            return Err(GpuError::NoDecodeQueue);
        }
        let profile = Profile::new(bit_depth, film_grain);
        Self::for_profile(device, &profile)
    }

    pub(crate) fn for_profile(device: &Device, profile: &Profile) -> Result<Self, GpuError> {
        let functions = video_queue::Instance::new(device.entry(), device.instance_raw());
        let physical = device.physical();
        let mut av1 = vk::VideoDecodeAV1CapabilitiesKHR::default();
        let mut decode = vk::VideoDecodeCapabilitiesKHR::default();
        let (result, caps) = {
            let mut caps = vk::VideoCapabilitiesKHR::default()
                .push_next(&mut decode)
                .push_next(&mut av1);
            // SAFETY: the physical device belongs to the instance; the profile and the
            // output chain are valid structures.
            let result = unsafe {
                (functions.fp().get_physical_device_video_capabilities_khr)(
                    physical,
                    profile.info(),
                    &mut caps,
                )
            };
            (
                result,
                (
                    caps.min_coded_extent,
                    caps.max_coded_extent,
                    caps.max_dpb_slots,
                    caps.max_active_reference_pictures,
                    caps.picture_access_granularity,
                    caps.min_bitstream_buffer_offset_alignment,
                    caps.min_bitstream_buffer_size_alignment,
                    caps.std_header_version,
                ),
            )
        };
        match result {
            vk::Result::SUCCESS => {}
            vk::Result::ERROR_VIDEO_PROFILE_CODEC_NOT_SUPPORTED_KHR
            | vk::Result::ERROR_VIDEO_PROFILE_FORMAT_NOT_SUPPORTED_KHR
            | vk::Result::ERROR_VIDEO_PICTURE_LAYOUT_NOT_SUPPORTED_KHR
            | vk::Result::ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR => {
                return Err(GpuError::Unsupported(
                    "the profile (bit depth or film grain)",
                ));
            }
            result => {
                return Err(GpuError::Vulkan {
                    call: "vkGetPhysicalDeviceVideoCapabilitiesKHR",
                    result,
                });
            }
        }
        let (min_extent, max_extent, max_dpb_slots, max_active, granularity, offset, size, std) =
            caps;
        let coincide = decode
            .flags
            .contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE);
        let distinct = decode
            .flags
            .contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT);
        // Coinciding, one image is both: its format must take both usages.
        let dpb_wanted = if coincide && !distinct {
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
        } else {
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR
        };
        let dpb = format(&functions, physical, profile, dpb_wanted)?;
        let output = format(
            &functions,
            physical,
            profile,
            vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
        )?;
        if dpb.format != output.format {
            return Err(GpuError::Unsupported(
                "reference and output pictures of different formats",
            ));
        }
        Ok(Self {
            coincide,
            distinct,
            max_level: av1.max_level as u8,
            min_extent,
            max_extent,
            max_dpb_slots,
            max_active_references: max_active,
            granularity,
            bitstream_offset_alignment: offset.max(1),
            bitstream_size_alignment: size.max(1),
            std_header_version: std,
            format: dpb.format,
            dpb_flags: dpb.image_create_flags,
            dpb_usage: dpb.image_usage_flags,
            output_flags: output.image_create_flags,
            output_usage: output.image_usage_flags,
        })
    }

    /// Why the decoder cannot decode a stream with this sequence header, if it cannot.
    pub fn refuses(&self, sequence: &SequenceHeader) -> Option<&'static str> {
        let c = &sequence.color;
        if sequence.seq_profile != 0 || c.mono_chrome || !(c.subsampling_x && c.subsampling_y) {
            return Some("only AV1's main profile in 4:2:0");
        }
        let level = sequence.operating_points[0].seq_level_idx;
        if level != LEVEL_MAX_PARAMETERS && level > self.max_level {
            return Some("the stream's level is above the decoder's");
        }
        if sequence.max_width() > self.max_extent.width
            || sequence.max_height() > self.max_extent.height
        {
            return Some("pictures larger than the decoder takes");
        }
        None
    }
}

/// The first format the decoder offers for pictures with `usage` (tiling optimal).
fn format(
    functions: &video_queue::Instance,
    physical: vk::PhysicalDevice,
    profile: &Profile,
    usage: vk::ImageUsageFlags,
) -> Result<vk::VideoFormatPropertiesKHR<'static>, GpuError> {
    let profiles = [*profile.info()];
    let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
    let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(usage)
        .push_next(&mut list);
    let query = |count: &mut u32, out: *mut vk::VideoFormatPropertiesKHR<'_>| {
        // SAFETY: the physical device belongs to the instance; `out` is null or holds
        // `count` structures.
        unsafe {
            (functions
                .fp()
                .get_physical_device_video_format_properties_khr)(
                physical, &info, count, out
            )
        }
    };
    let mut count = 0u32;
    match query(&mut count, core::ptr::null_mut()) {
        vk::Result::SUCCESS => {}
        vk::Result::ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR => {
            return Err(GpuError::Unsupported("picture images of the usage needed"));
        }
        result => {
            return Err(GpuError::Vulkan {
                call: "vkGetPhysicalDeviceVideoFormatPropertiesKHR",
                result,
            });
        }
    }
    let mut formats = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    let result = query(&mut count, formats.as_mut_ptr());
    if result != vk::Result::SUCCESS && result != vk::Result::INCOMPLETE {
        return Err(GpuError::Vulkan {
            call: "vkGetPhysicalDeviceVideoFormatPropertiesKHR",
            result,
        });
    }
    formats.truncate(count as usize);
    formats
        .into_iter()
        .find(|f| {
            f.image_tiling == vk::ImageTiling::OPTIMAL && f.image_type == vk::ImageType::TYPE_2D
        })
        .map(|f| vk::VideoFormatPropertiesKHR {
            p_next: core::ptr::null_mut(),
            ..f
        })
        .ok_or(GpuError::Unsupported("no optimal-tiling picture format"))
}
