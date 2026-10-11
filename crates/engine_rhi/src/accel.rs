//! Acceleration structures for ray queries (ADR 0087): what the device enabled
//! ([`RayQuery`], from [`crate::Device::ray_query`]) and the structures themselves
//! ([`AccelerationStructure`]), each in a device-local buffer of its own.
//!
//! Shaders reach a structure by its device address ([`AccelerationStructure::address`]),
//! pushed like any other address and turned into a `RaytracingAccelerationStructure` with
//! `OpConvertUToAccelerationStructureKHR` (`accel_at` in `engine_render`'s shaders): no
//! descriptor binding is needed. Builds are recorded by their owners
//! (`vkCmdBuildAccelerationStructuresKHR` through [`RayQuery::loader`]).

use crate::{Allocation, Allocator, Device, MemoryLocation, RhiError};
use ash::vk;

/// Ray queries and acceleration structures, as the device enabled them
/// (`VK_KHR_acceleration_structure`, `VK_KHR_ray_query`).
pub struct RayQuery {
    loader: ash::khr::acceleration_structure::Device,
    scratch_alignment: u64,
    max_instances: u64,
    max_primitives: u64,
    position_fetch: bool,
}

impl core::fmt::Debug for RayQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RayQuery")
            .field("scratch_alignment", &self.scratch_alignment)
            .field("max_instances", &self.max_instances)
            .field("max_primitives", &self.max_primitives)
            .field("position_fetch", &self.position_fetch)
            .finish_non_exhaustive()
    }
}

impl RayQuery {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical: vk::PhysicalDevice,
        position_fetch: bool,
    ) -> Self {
        let mut properties = vk::PhysicalDeviceAccelerationStructurePropertiesKHR::default();
        let mut query = vk::PhysicalDeviceProperties2::default().push_next(&mut properties);
        // SAFETY: the structure belongs to an extension the device enabled.
        unsafe { instance.get_physical_device_properties2(physical, &mut query) };
        Self {
            loader: ash::khr::acceleration_structure::Device::new(instance, device),
            scratch_alignment: u64::from(
                properties
                    .min_acceleration_structure_scratch_offset_alignment
                    .max(1),
            ),
            max_instances: properties.max_instance_count,
            max_primitives: properties.max_primitive_count,
            position_fetch,
        }
    }

    /// The extension's functions: builds, copies, build sizes, compacted sizes.
    pub fn loader(&self) -> &ash::khr::acceleration_structure::Device {
        &self.loader
    }

    /// Alignment of scratch buffer addresses.
    pub fn scratch_alignment(&self) -> u64 {
        self.scratch_alignment
    }

    /// Most instances in a top-level structure.
    pub fn max_instances(&self) -> u64 {
        self.max_instances
    }

    /// Most triangles in a bottom-level structure.
    pub fn max_primitives(&self) -> u64 {
        self.max_primitives
    }

    /// Whether `VK_KHR_ray_tracing_position_fetch` is enabled: ray queries can read the
    /// hit triangle's positions from the structure.
    pub fn position_fetch(&self) -> bool {
        self.position_fetch
    }
}

/// A top-level (instances) or bottom-level (triangles) structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelLevel {
    /// Instances of bottom-level structures.
    Top,
    /// Triangles.
    Bottom,
}

impl AccelLevel {
    /// The Vulkan type.
    pub fn raw(self) -> vk::AccelerationStructureTypeKHR {
        match self {
            Self::Top => vk::AccelerationStructureTypeKHR::TOP_LEVEL,
            Self::Bottom => vk::AccelerationStructureTypeKHR::BOTTOM_LEVEL,
        }
    }
}

/// An acceleration structure in a device-local buffer of its own (so its offset is 0,
/// which every alignment rule allows).
#[derive(Debug)]
pub struct AccelerationStructure {
    raw: vk::AccelerationStructureKHR,
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    size: u64,
    level: AccelLevel,
}

impl AccelerationStructure {
    /// Creates a structure of `size` bytes (a build size's
    /// `acceleration_structure_size`, or a compacted size), not built yet.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        level: AccelLevel,
        size: u64,
    ) -> Result<Self, RhiError> {
        let ray_query = device.ray_query().ok_or(RhiError::Vulkan {
            call: "acceleration structures (ray queries are not enabled)",
            result: vk::Result::ERROR_FEATURE_NOT_PRESENT,
        })?;
        let size = size.max(256);
        let (buffer, allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(size).usage(
                vk::BufferUsageFlags::ACCELERATION_STRUCTURE_STORAGE_KHR
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
            ),
            MemoryLocation::Device,
        )?;
        let info = vk::AccelerationStructureCreateInfoKHR::default()
            .buffer(buffer)
            .offset(0)
            .size(size)
            .ty(level.raw());
        // SAFETY: the buffer was made with the storage usage and bound; offset 0.
        let raw = match unsafe { ray_query.loader.create_acceleration_structure(&info, None) } {
            Ok(raw) => raw,
            Err(result) => {
                // SAFETY: nothing uses the fresh buffer.
                unsafe { allocator.destroy_buffer(buffer, allocation) };
                return Err(RhiError::Vulkan {
                    call: "vkCreateAccelerationStructureKHR",
                    result,
                });
            }
        };
        // SAFETY: the structure was just created.
        let address = unsafe {
            ray_query.loader.get_acceleration_structure_device_address(
                &vk::AccelerationStructureDeviceAddressInfoKHR::default()
                    .acceleration_structure(raw),
            )
        };
        Ok(Self {
            raw,
            buffer,
            allocation: Some(allocation),
            address,
            size,
            level,
        })
    }

    /// The Vulkan handle.
    pub fn raw(&self) -> vk::AccelerationStructureKHR {
        self.raw
    }

    /// Its device address: what instances of a top-level structure and shaders refer to.
    pub fn address(&self) -> vk::DeviceAddress {
        self.address
    }

    /// The buffer it lives in (for barriers).
    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// Bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Top or bottom level.
    pub fn level(&self) -> AccelLevel {
        self.level
    }

    /// Destroys the structure and frees its buffer.
    ///
    /// # Safety
    ///
    /// No pending GPU work uses it.
    pub unsafe fn destroy(&mut self, device: &Device, allocator: &mut Allocator) {
        let Some(allocation) = self.allocation.take() else {
            return;
        };
        if let Some(ray_query) = device.ray_query() {
            // SAFETY: guaranteed by the caller.
            unsafe {
                ray_query
                    .loader
                    .destroy_acceleration_structure(self.raw, None);
            }
        }
        // SAFETY: guaranteed by the caller.
        unsafe { allocator.destroy_buffer(self.buffer, allocation) };
        self.raw = vk::AccelerationStructureKHR::null();
        self.buffer = vk::Buffer::null();
    }
}
