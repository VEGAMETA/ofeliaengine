//! The part of the Streamline ABI the engine uses (Streamline SDK 2.14.1, MIT), declared
//! from its public headers `include/sl_*.h`: the structures' layouts, the GUIDs that type
//! them, the enumerations' values and the functions' signatures. No header is compiled;
//! the layouts are checked against the offsets Streamline itself asserts
//! (`source/core/sl.api/sl.cpp`) in this module's tests.
//!
//! Streamline's structures start with `sl::BaseStructure` (`sl_struct.h`): a `next`
//! pointer, a GUID and a version. New members are only ever appended, and the version
//! bumped, so a newer Streamline reads these older versions (`sl_struct.h`).

#![allow(dead_code)] // the ABI is declared whole; the engine uses part of it

use core::ffi::{c_char, c_void};

/// `sl::StructType`: a GUID.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StructType {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> StructType {
    StructType {
        data1,
        data2,
        data3,
        data4,
    }
}

/// `sl::BaseStructure`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct BaseStructure {
    pub next: *mut BaseStructure,
    pub struct_type: StructType,
    pub struct_version: usize,
}

impl BaseStructure {
    const fn new(struct_type: StructType, struct_version: usize) -> Self {
        Self {
            next: core::ptr::null_mut(),
            struct_type,
            struct_version,
        }
    }
}

/// `sl::Result` (`sl_result.h`).
pub(crate) type SlResult = i32;
/// `sl::Result::eOk`.
pub(crate) const OK: SlResult = 0;

/// Names of `sl::Result` values, by value (`sl_result.h`).
pub(crate) const RESULT_NAMES: [&str; 40] = [
    "eOk",
    "eErrorIO",
    "eErrorDriverOutOfDate",
    "eErrorOSOutOfDate",
    "eErrorOSDisabledHWS",
    "eErrorDeviceNotCreated",
    "eErrorNoSupportedAdapterFound",
    "eErrorAdapterNotSupported",
    "eErrorNoPlugins",
    "eErrorVulkanAPI",
    "eErrorDXGIAPI",
    "eErrorD3DAPI",
    "eErrorNRDAPI",
    "eErrorNVAPI",
    "eErrorReflexAPI",
    "eErrorNGXFailed",
    "eErrorJSONParsing",
    "eErrorMissingProxy",
    "eErrorMissingResourceState",
    "eErrorInvalidIntegration",
    "eErrorMissingInputParameter",
    "eErrorNotInitialized",
    "eErrorComputeFailed",
    "eErrorInitNotCalled",
    "eErrorExceptionHandler",
    "eErrorInvalidParameter",
    "eErrorMissingConstants",
    "eErrorDuplicatedConstants",
    "eErrorMissingOrInvalidAPI",
    "eErrorCommonConstantsMissing",
    "eErrorUnsupportedInterface",
    "eErrorFeatureMissing",
    "eErrorFeatureNotSupported",
    "eErrorFeatureMissingHooks",
    "eErrorFeatureFailedToLoad",
    "eErrorFeatureWrongPriority",
    "eErrorFeatureMissingDependency",
    "eErrorFeatureManagerInvalidState",
    "eErrorInvalidState",
    "eWarnOutOfVRAM",
];

/// `sl::kSDKVersion` of the headers these declarations follow (`sl_version.h`): 2.14.1.
pub(crate) const SDK_VERSION: u64 = (2 << 48) | (14 << 32) | (1 << 16) | 0xfedc;

/// `sl::Feature` (`sl_core_types.h`).
pub(crate) type Feature = u32;
/// `sl::kFeatureDLSS`.
pub(crate) const FEATURE_DLSS: Feature = 0;

/// `sl::BufferType` values (`sl_core_types.h`).
pub(crate) const BUFFER_DEPTH: u32 = 0;
pub(crate) const BUFFER_MOTION_VECTORS: u32 = 1;
pub(crate) const BUFFER_SCALING_INPUT_COLOR: u32 = 3;
pub(crate) const BUFFER_SCALING_OUTPUT_COLOR: u32 = 4;
pub(crate) const BUFFER_EXPOSURE: u32 = 13;
pub(crate) const BUFFER_TRANSPARENCY_HINT: u32 = 27;
pub(crate) const BUFFER_BIAS_CURRENT_COLOR_HINT: u32 = 29;

/// `sl::LogLevel`.
pub(crate) const LOG_LEVEL_OFF: u32 = 0;
pub(crate) const LOG_LEVEL_DEFAULT: u32 = 1;
pub(crate) const LOG_LEVEL_VERBOSE: u32 = 2;

/// `sl::LogType`.
pub(crate) const LOG_INFO: u32 = 0;
pub(crate) const LOG_WARN: u32 = 1;
pub(crate) const LOG_ERROR: u32 = 2;

/// `sl::PreferenceFlags`.
pub(crate) const PREFERENCE_DISABLE_CL_STATE_TRACKING: u64 = 1 << 0;
pub(crate) const PREFERENCE_DISABLE_DEBUG_TEXT: u64 = 1 << 1;
pub(crate) const PREFERENCE_USE_MANUAL_HOOKING: u64 = 1 << 2;
pub(crate) const PREFERENCE_ALLOW_OTA: u64 = 1 << 3;
pub(crate) const PREFERENCE_LOAD_DOWNLOADED_PLUGINS: u64 = 1 << 6;
pub(crate) const PREFERENCE_FRAME_BASED_TAGGING: u64 = 1 << 7;

/// `sl::EngineType::eCustom`.
pub(crate) const ENGINE_CUSTOM: u32 = 0;
/// `sl::RenderAPI::eVulkan`.
pub(crate) const RENDER_API_VULKAN: u32 = 2;

/// `sl::ResourceType::eTex2d` (a `char`).
pub(crate) const RESOURCE_TEX2D: u8 = 0;

/// `sl::ResourceLifecycle`.
pub(crate) const LIFECYCLE_ONLY_VALID_NOW: u32 = 0;
pub(crate) const LIFECYCLE_VALID_UNTIL_PRESENT: u32 = 1;
pub(crate) const LIFECYCLE_VALID_UNTIL_EVALUATE: u32 = 2;

/// `sl::Boolean` (a `char`).
pub(crate) const FALSE: u8 = 0;
pub(crate) const TRUE: u8 = 1;
pub(crate) const INVALID: u8 = 2;

/// `sl::FeatureRequirementFlags::eVulkanSupported`.
pub(crate) const REQUIREMENT_VULKAN: u32 = 1 << 2;

/// `sl::INVALID_FLOAT`.
pub(crate) const INVALID_FLOAT: f32 = f32::MAX;

/// `sl::Preferences` (version 1).
#[repr(C)]
pub(crate) struct Preferences {
    pub base: BaseStructure,
    pub show_console: bool,
    pub log_level: u32,
    pub paths_to_plugins: *const *const u16,
    pub num_paths_to_plugins: u32,
    pub path_to_logs_and_data: *const u16,
    pub allocate_callback: *const c_void,
    pub release_callback: *const c_void,
    pub log_message_callback: Option<LogCallback>,
    pub flags: u64,
    pub features_to_load: *const Feature,
    pub num_features_to_load: u32,
    pub application_id: u32,
    pub engine: u32,
    pub engine_version: *const c_char,
    pub project_id: *const c_char,
    pub render_api: u32,
}

impl Preferences {
    pub(crate) const TYPE: StructType = guid(
        0x1ca1_0965,
        0xbf8e,
        0x432b,
        [0x8d, 0xa1, 0x67, 0x16, 0xd8, 0x79, 0xfb, 0x14],
    );

    pub(crate) fn new() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 1),
            show_console: false,
            log_level: LOG_LEVEL_DEFAULT,
            paths_to_plugins: core::ptr::null(),
            num_paths_to_plugins: 0,
            path_to_logs_and_data: core::ptr::null(),
            allocate_callback: core::ptr::null(),
            release_callback: core::ptr::null(),
            log_message_callback: None,
            flags: 0,
            features_to_load: core::ptr::null(),
            num_features_to_load: 0,
            application_id: 0,
            engine: ENGINE_CUSTOM,
            engine_version: core::ptr::null(),
            project_id: core::ptr::null(),
            render_api: RENDER_API_VULKAN,
        }
    }
}

/// `sl::PFun_LogMessageCallback`.
pub(crate) type LogCallback = unsafe extern "C" fn(kind: u32, message: *const c_char);

/// `sl::Version`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Version {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

/// `sl::FeatureRequirements` (version 2).
#[repr(C)]
pub(crate) struct FeatureRequirements {
    pub base: BaseStructure,
    pub flags: u32,
    pub max_num_cpu_threads: u32,
    pub max_num_viewports: u32,
    pub num_required_tags: u32,
    pub required_tags: *const u32,
    pub os_version_detected: Version,
    pub os_version_required: Version,
    pub driver_version_detected: Version,
    pub driver_version_required: Version,
    pub vk_num_compute_queues_required: u32,
    pub vk_num_graphics_queues_required: u32,
    pub vk_num_device_extensions: u32,
    pub vk_device_extensions: *const *const c_char,
    pub vk_num_instance_extensions: u32,
    pub vk_instance_extensions: *const *const c_char,
    pub vk_num_features12: u32,
    pub vk_features12: *const *const c_char,
    pub vk_num_features13: u32,
    pub vk_features13: *const *const c_char,
    pub vk_num_optical_flow_queues_required: u32,
}

impl FeatureRequirements {
    pub(crate) const TYPE: StructType = guid(
        0x6671_4097,
        0xac6d,
        0x4bc6,
        [0x89, 0x15, 0x1e, 0x0f, 0x55, 0xa6, 0xb6, 0x1f],
    );

    pub(crate) fn new() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 2),
            flags: 0,
            max_num_cpu_threads: 0,
            max_num_viewports: 0,
            num_required_tags: 0,
            required_tags: core::ptr::null(),
            os_version_detected: Version::default(),
            os_version_required: Version::default(),
            driver_version_detected: Version::default(),
            driver_version_required: Version::default(),
            vk_num_compute_queues_required: 0,
            vk_num_graphics_queues_required: 0,
            vk_num_device_extensions: 0,
            vk_device_extensions: core::ptr::null(),
            vk_num_instance_extensions: 0,
            vk_instance_extensions: core::ptr::null(),
            vk_num_features12: 0,
            vk_features12: core::ptr::null(),
            vk_num_features13: 0,
            vk_features13: core::ptr::null(),
            vk_num_optical_flow_queues_required: 0,
        }
    }
}

/// `sl::FeatureVersion`.
#[repr(C)]
pub(crate) struct FeatureVersion {
    pub base: BaseStructure,
    pub version_sl: Version,
    pub version_ngx: Version,
}

impl FeatureVersion {
    pub(crate) const TYPE: StructType = guid(
        0x6d5b_51f0,
        0x076b,
        0x486d,
        [0x99, 0x95, 0x5a, 0x56, 0x10, 0x43, 0xf5, 0xc1],
    );

    pub(crate) fn new() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 1),
            version_sl: Version::default(),
            version_ngx: Version::default(),
        }
    }
}

/// `sl::AdapterInfo`.
#[repr(C)]
pub(crate) struct AdapterInfo {
    pub base: BaseStructure,
    pub device_luid: *const u8,
    pub device_luid_size: u32,
    pub vk_physical_device: *mut c_void,
}

impl AdapterInfo {
    pub(crate) const TYPE: StructType = guid(
        0x0677_315f,
        0xa746,
        0x4492,
        [0x9f, 0x42, 0xcb, 0x61, 0x42, 0xc9, 0xc3, 0xd4],
    );

    pub(crate) fn new() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 1),
            device_luid: core::ptr::null(),
            device_luid_size: 0,
            vk_physical_device: core::ptr::null_mut(),
        }
    }
}

/// `sl::VulkanInfo` (version 3, `sl_helpers_vk.h`).
#[repr(C)]
pub(crate) struct VulkanInfo {
    pub base: BaseStructure,
    pub device: *mut c_void,
    pub instance: *mut c_void,
    pub physical_device: *mut c_void,
    pub compute_queue_index: u32,
    pub compute_queue_family: u32,
    pub graphics_queue_index: u32,
    pub graphics_queue_family: u32,
    pub optical_flow_queue_index: u32,
    pub optical_flow_queue_family: u32,
    pub use_native_optical_flow_mode: bool,
    pub compute_queue_create_flags: u32,
    pub graphics_queue_create_flags: u32,
    pub optical_flow_queue_create_flags: u32,
}

impl VulkanInfo {
    pub(crate) const TYPE: StructType = guid(
        0x0eed_6fd5,
        0x82cd,
        0x43a9,
        [0xbd, 0xb5, 0x47, 0xa5, 0xba, 0x2f, 0x45, 0xd6],
    );

    pub(crate) fn new() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 3),
            device: core::ptr::null_mut(),
            instance: core::ptr::null_mut(),
            physical_device: core::ptr::null_mut(),
            compute_queue_index: 0,
            compute_queue_family: 0,
            graphics_queue_index: 0,
            graphics_queue_family: 0,
            optical_flow_queue_index: 0,
            optical_flow_queue_family: 0,
            use_native_optical_flow_mode: false,
            compute_queue_create_flags: 0,
            graphics_queue_create_flags: 0,
            optical_flow_queue_create_flags: 0,
        }
    }
}

/// `sl::ViewportHandle`.
#[repr(C)]
pub(crate) struct ViewportHandle {
    pub base: BaseStructure,
    pub value: u32,
}

impl ViewportHandle {
    pub(crate) const TYPE: StructType = guid(
        0x171b_6435,
        0x9b3c,
        0x4fc8,
        [0x99, 0x94, 0xfb, 0xe5, 0x25, 0x69, 0xaa, 0xa4],
    );

    pub(crate) fn new(value: u32) -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 1),
            value,
        }
    }
}

/// `sl::FrameToken`: an object Streamline owns (a C++ class with a virtual function); the
/// engine only passes its address back.
#[repr(C)]
pub(crate) struct FrameToken {
    _opaque: [u8; 0],
}

/// `sl::Extent`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Extent {
    pub top: u32,
    pub left: u32,
    pub width: u32,
    pub height: u32,
}

/// `sl::Resource`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Resource {
    pub base: BaseStructure,
    pub kind: u8,
    pub native: *mut c_void,
    pub memory: *mut c_void,
    pub view: *mut c_void,
    /// `VkImageLayout` for Vulkan.
    pub state: u32,
    pub width: u32,
    pub height: u32,
    /// `VkFormat`.
    pub native_format: u32,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub gpu_virtual_address: u64,
    /// `VkImageCreateFlags`.
    pub flags: u32,
    /// `VkImageUsageFlags`.
    pub usage: u32,
    pub internal_flags: u16,
    pub reserved: u16,
}

impl Resource {
    pub(crate) const TYPE: StructType = guid(
        0x3a9d_70cf,
        0x2418,
        0x4b72,
        [0x83, 0x91, 0x13, 0xf8, 0x72, 0x1c, 0x72, 0x61],
    );

    pub(crate) const fn empty() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 1),
            kind: RESOURCE_TEX2D,
            native: core::ptr::null_mut(),
            memory: core::ptr::null_mut(),
            view: core::ptr::null_mut(),
            state: u32::MAX,
            width: 0,
            height: 0,
            native_format: 0,
            mip_levels: 0,
            array_layers: 0,
            gpu_virtual_address: 0,
            flags: 0,
            usage: 0,
            internal_flags: 0,
            reserved: 0,
        }
    }
}

/// `sl::ResourceTag`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ResourceTag {
    pub base: BaseStructure,
    pub resource: *mut Resource,
    pub kind: u32,
    pub lifecycle: u32,
    pub extent: Extent,
}

impl ResourceTag {
    pub(crate) const TYPE: StructType = guid(
        0x4c6a_5aad,
        0xb445,
        0x496c,
        [0x87, 0xff, 0x1a, 0xf3, 0x84, 0x5b, 0xe6, 0x53],
    );

    pub(crate) const fn empty() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 1),
            resource: core::ptr::null_mut(),
            kind: 0,
            lifecycle: LIFECYCLE_VALID_UNTIL_EVALUATE,
            extent: Extent {
                top: 0,
                left: 0,
                width: 0,
                height: 0,
            },
        }
    }
}

/// `sl::float4x4`: row major.
pub(crate) type Float4x4 = [[f32; 4]; 4];

/// `sl::Constants` (version 2, `sl_consts.h`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Constants {
    pub base: BaseStructure,
    pub camera_view_to_clip: Float4x4,
    pub clip_to_camera_view: Float4x4,
    pub clip_to_lens_clip: Float4x4,
    pub clip_to_prev_clip: Float4x4,
    pub prev_clip_to_clip: Float4x4,
    pub jitter_offset: [f32; 2],
    pub mvec_scale: [f32; 2],
    pub camera_pinhole_offset: [f32; 2],
    pub camera_pos: [f32; 3],
    pub camera_up: [f32; 3],
    pub camera_right: [f32; 3],
    pub camera_fwd: [f32; 3],
    pub camera_near: f32,
    pub camera_far: f32,
    pub camera_fov: f32,
    pub camera_aspect_ratio: f32,
    pub motion_vectors_invalid_value: f32,
    pub depth_inverted: u8,
    pub camera_motion_included: u8,
    pub motion_vectors_3d: u8,
    pub reset: u8,
    pub orthographic_projection: u8,
    pub motion_vectors_dilated: u8,
    pub motion_vectors_jittered: u8,
    pub min_relative_linear_depth_object_separation: f32,
}

impl Constants {
    pub(crate) const TYPE: StructType = guid(
        0xdcd3_5ad7,
        0x4e4a,
        0x4bad,
        [0xa9, 0x0c, 0xe0, 0xc4, 0x9e, 0xb2, 0x3a, 0xfe],
    );

    /// The constructor's defaults (`INVALID_FLOAT` everywhere, `eInvalid` booleans), with
    /// identity matrices.
    pub(crate) fn new() -> Self {
        const IDENTITY: Float4x4 = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        Self {
            base: BaseStructure::new(Self::TYPE, 2),
            camera_view_to_clip: IDENTITY,
            clip_to_camera_view: IDENTITY,
            clip_to_lens_clip: IDENTITY,
            clip_to_prev_clip: IDENTITY,
            prev_clip_to_clip: IDENTITY,
            jitter_offset: [0.0; 2],
            mvec_scale: [1.0; 2],
            camera_pinhole_offset: [0.0; 2],
            camera_pos: [0.0; 3],
            camera_up: [0.0, 1.0, 0.0],
            camera_right: [1.0, 0.0, 0.0],
            camera_fwd: [0.0, 0.0, 1.0],
            camera_near: INVALID_FLOAT,
            camera_far: INVALID_FLOAT,
            camera_fov: INVALID_FLOAT,
            camera_aspect_ratio: INVALID_FLOAT,
            motion_vectors_invalid_value: INVALID_FLOAT,
            depth_inverted: INVALID,
            camera_motion_included: INVALID,
            motion_vectors_3d: INVALID,
            reset: INVALID,
            orthographic_projection: FALSE,
            motion_vectors_dilated: FALSE,
            motion_vectors_jittered: FALSE,
            min_relative_linear_depth_object_separation: 40.0,
        }
    }
}

/// `sl::DLSSMode` (`sl_dlss.h`).
pub(crate) const DLSS_MODE_OFF: u32 = 0;
pub(crate) const DLSS_MODE_MAX_PERFORMANCE: u32 = 1;
pub(crate) const DLSS_MODE_BALANCED: u32 = 2;
pub(crate) const DLSS_MODE_MAX_QUALITY: u32 = 3;
pub(crate) const DLSS_MODE_ULTRA_PERFORMANCE: u32 = 4;
pub(crate) const DLSS_MODE_ULTRA_QUALITY: u32 = 5;
pub(crate) const DLSS_MODE_DLAA: u32 = 6;

/// `sl::DLSSOptions` (version 3).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct DlssOptions {
    pub base: BaseStructure,
    pub mode: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub sharpness: f32,
    pub pre_exposure: f32,
    pub exposure_scale: f32,
    pub color_buffers_hdr: u8,
    pub indicator_invert_axis_x: u8,
    pub indicator_invert_axis_y: u8,
    pub dlaa_preset: u32,
    pub quality_preset: u32,
    pub balanced_preset: u32,
    pub performance_preset: u32,
    pub ultra_performance_preset: u32,
    pub ultra_quality_preset: u32,
    pub use_auto_exposure: u8,
    pub alpha_upscaling_enabled: u8,
}

impl DlssOptions {
    pub(crate) const TYPE: StructType = guid(
        0x6ac8_26e4,
        0x4c61,
        0x4101,
        [0xa9, 0x2d, 0x63, 0x8d, 0x42, 0x10, 0x57, 0xb8],
    );

    pub(crate) fn new() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 3),
            mode: DLSS_MODE_OFF,
            output_width: u32::MAX,
            output_height: u32::MAX,
            sharpness: 0.0,
            pre_exposure: 1.0,
            exposure_scale: 1.0,
            color_buffers_hdr: TRUE,
            indicator_invert_axis_x: FALSE,
            indicator_invert_axis_y: FALSE,
            dlaa_preset: 0,
            quality_preset: 0,
            balanced_preset: 0,
            performance_preset: 0,
            ultra_performance_preset: 0,
            ultra_quality_preset: 0,
            use_auto_exposure: FALSE,
            alpha_upscaling_enabled: FALSE,
        }
    }
}

/// `sl::DLSSOptimalSettings`.
#[repr(C)]
pub(crate) struct DlssOptimalSettings {
    pub base: BaseStructure,
    pub optimal_render_width: u32,
    pub optimal_render_height: u32,
    pub optimal_sharpness: f32,
    pub render_width_min: u32,
    pub render_height_min: u32,
    pub render_width_max: u32,
    pub render_height_max: u32,
}

impl DlssOptimalSettings {
    pub(crate) const TYPE: StructType = guid(
        0xef1d_0957,
        0xfd58,
        0x4df7,
        [0xb5, 0x04, 0x8b, 0x69, 0xd8, 0xaa, 0x6b, 0x76],
    );

    pub(crate) fn new() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 1),
            optimal_render_width: 0,
            optimal_render_height: 0,
            optimal_sharpness: 0.0,
            render_width_min: 0,
            render_height_min: 0,
            render_width_max: 0,
            render_height_max: 0,
        }
    }
}

/// `sl::DLSSState`.
#[repr(C)]
pub(crate) struct DlssState {
    pub base: BaseStructure,
    pub estimated_vram_usage_in_bytes: u64,
}

impl DlssState {
    pub(crate) const TYPE: StructType = guid(
        0x9366_b056,
        0x8c01,
        0x463c,
        [0xbb, 0x91, 0xe6, 0x87, 0x82, 0x63, 0x6c, 0xe9],
    );

    pub(crate) fn new() -> Self {
        Self {
            base: BaseStructure::new(Self::TYPE, 1),
            estimated_vram_usage_in_bytes: 0,
        }
    }
}

// Function signatures (`sl_core_api.h`, `sl_helpers_vk.h`, `sl_dlss.h`). C++ references
// are pointers in the ABI; every function returns `sl::Result`.
pub(crate) type PfnInit = unsafe extern "C" fn(*const Preferences, u64) -> SlResult;
pub(crate) type PfnShutdown = unsafe extern "C" fn() -> SlResult;
pub(crate) type PfnIsFeatureSupported =
    unsafe extern "C" fn(Feature, *const AdapterInfo) -> SlResult;
pub(crate) type PfnGetFeatureRequirements =
    unsafe extern "C" fn(Feature, *mut FeatureRequirements) -> SlResult;
pub(crate) type PfnGetFeatureVersion =
    unsafe extern "C" fn(Feature, *mut FeatureVersion) -> SlResult;
pub(crate) type PfnSetVulkanInfo = unsafe extern "C" fn(*const VulkanInfo) -> SlResult;
pub(crate) type PfnGetNewFrameToken =
    unsafe extern "C" fn(*mut *mut FrameToken, *const u32) -> SlResult;
pub(crate) type PfnSetConstants =
    unsafe extern "C" fn(*const Constants, *const FrameToken, *const ViewportHandle) -> SlResult;
pub(crate) type PfnEvaluateFeature = unsafe extern "C" fn(
    Feature,
    *const FrameToken,
    *const *const BaseStructure,
    u32,
    *mut c_void,
) -> SlResult;
pub(crate) type PfnFreeResources = unsafe extern "C" fn(Feature, *const ViewportHandle) -> SlResult;
pub(crate) type PfnGetFeatureFunction =
    unsafe extern "C" fn(Feature, *const c_char, *mut *mut c_void) -> SlResult;
pub(crate) type PfnDlssGetOptimalSettings =
    unsafe extern "C" fn(*const DlssOptions, *mut DlssOptimalSettings) -> SlResult;
pub(crate) type PfnDlssGetState =
    unsafe extern "C" fn(*const ViewportHandle, *mut DlssState) -> SlResult;
pub(crate) type PfnDlssSetOptions =
    unsafe extern "C" fn(*const ViewportHandle, *const DlssOptions) -> SlResult;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    /// The offsets Streamline 2.14.1 asserts in `source/core/sl.api/sl.cpp`.
    #[test]
    fn layouts_match_streamline() {
        assert_eq!(size_of::<BaseStructure>(), 32);
        assert_eq!(offset_of!(Preferences, render_api), 136);
        assert_eq!(offset_of!(ResourceTag, extent), 48);
        assert_eq!(offset_of!(Resource, internal_flags), 104);
        assert_eq!(offset_of!(Resource, reserved), 106);
        assert_eq!(offset_of!(Constants, motion_vectors_jittered), 450);
        assert_eq!(offset_of!(VulkanInfo, use_native_optical_flow_mode), 80);
        assert_eq!(offset_of!(AdapterInfo, vk_physical_device), 48);
        assert_eq!(offset_of!(FeatureVersion, version_ngx), 44);
        assert_eq!(
            offset_of!(FeatureRequirements, vk_num_optical_flow_queues_required),
            176
        );
    }

    /// Offsets derived from the headers' declarations (C layout rules), for the structures
    /// Streamline does not assert.
    #[test]
    fn other_layouts() {
        assert_eq!(offset_of!(Constants, jitter_offset), 352);
        assert_eq!(offset_of!(Constants, camera_near), 424);
        assert_eq!(
            offset_of!(Constants, min_relative_linear_depth_object_separation),
            452
        );
        assert_eq!(size_of::<Constants>(), 456);
        assert_eq!(offset_of!(DlssOptions, dlaa_preset), 60);
        assert_eq!(offset_of!(DlssOptions, alpha_upscaling_enabled), 85);
        assert_eq!(size_of::<DlssOptions>(), 88);
        assert_eq!(offset_of!(DlssOptimalSettings, render_height_max), 56);
        assert_eq!(offset_of!(DlssState, estimated_vram_usage_in_bytes), 32);
        assert_eq!(offset_of!(ViewportHandle, value), 32);
        assert_eq!(size_of::<Resource>(), 112);
        assert_eq!(size_of::<ResourceTag>(), 64);
        assert_eq!(offset_of!(VulkanInfo, optical_flow_queue_create_flags), 92);
    }

    #[test]
    fn sdk_version_is_2_14_1() {
        assert_eq!(SDK_VERSION >> 48, 2);
        assert_eq!((SDK_VERSION >> 32) & 0xffff, 14);
        assert_eq!((SDK_VERSION >> 16) & 0xffff, 1);
        assert_eq!(SDK_VERSION & 0xffff, 0xfedc);
    }
}
