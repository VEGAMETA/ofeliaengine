//! How a pass uses a resource, and what that means for synchronization2.

use engine_rhi::ash::vk;
use std::ops::BitOr;

/// Shader stages that access a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShaderStages(u8);

impl ShaderStages {
    /// Vertex shader.
    pub const VERTEX: Self = Self(1);
    /// Fragment shader.
    pub const FRAGMENT: Self = Self(2);
    /// Compute shader.
    pub const COMPUTE: Self = Self(4);
    /// Task (amplification) shader.
    pub const TASK: Self = Self(8);
    /// Mesh shader.
    pub const MESH: Self = Self(16);
    /// Every graphics stage, including task and mesh shaders (only valid on devices with
    /// mesh shaders enabled).
    pub const ALL_GRAPHICS: Self = Self(1 | 2 | 8 | 16);
    /// Vertex, fragment and compute: the stages every device has. Resources read by
    /// "any shader" (bindless textures) use this; task and mesh readers are added
    /// explicitly where mesh shaders are enabled.
    pub const ALL: Self = Self(1 | 2 | 4);

    pub(crate) fn pipeline_stages(self) -> vk::PipelineStageFlags2 {
        let mut stages = vk::PipelineStageFlags2::empty();
        for (bit, stage) in [
            (Self::VERTEX, vk::PipelineStageFlags2::VERTEX_SHADER),
            (Self::FRAGMENT, vk::PipelineStageFlags2::FRAGMENT_SHADER),
            (Self::COMPUTE, vk::PipelineStageFlags2::COMPUTE_SHADER),
            (Self::TASK, vk::PipelineStageFlags2::TASK_SHADER_EXT),
            (Self::MESH, vk::PipelineStageFlags2::MESH_SHADER_EXT),
        ] {
            if self.0 & bit.0 != 0 {
                stages |= stage;
            }
        }
        stages
    }
}

impl BitOr for ShaderStages {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// A use of a texture or buffer by a pass. Attachments are declared with
/// `PassBuilder::color` / `depth` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// Sampled image read.
    Sampled(ShaderStages),
    /// Storage image or storage buffer read.
    StorageRead(ShaderStages),
    /// Storage image or storage buffer write (may be partial: earlier contents are kept).
    StorageWrite(ShaderStages),
    /// Storage read and write.
    StorageReadWrite(ShaderStages),
    /// Uniform buffer read.
    Uniform(ShaderStages),
    /// Copy / blit source.
    TransferRead,
    /// Copy / blit / clear destination (may be partial).
    TransferWrite,
    /// Vertex buffer.
    VertexBuffer,
    /// Index buffer.
    IndexBuffer,
    /// Indirect draw / dispatch arguments.
    Indirect,
}

/// Synchronization facts about one access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccessInfo {
    pub(crate) stages: vk::PipelineStageFlags2,
    pub(crate) access: vk::AccessFlags2,
    /// Image layout (images only).
    pub(crate) layout: vk::ImageLayout,
    /// Reads what earlier passes wrote.
    pub(crate) reads: bool,
    /// Writes the resource.
    pub(crate) writes: bool,
    /// Overwrites the whole resource without reading it (attachments cleared or not
    /// loaded): earlier contents do not matter.
    pub(crate) discards: bool,
    pub(crate) image_usage: vk::ImageUsageFlags,
    pub(crate) buffer_usage: vk::BufferUsageFlags,
}

impl AccessInfo {
    fn new(
        stages: vk::PipelineStageFlags2,
        access: vk::AccessFlags2,
        layout: vk::ImageLayout,
        reads: bool,
        writes: bool,
    ) -> Self {
        Self {
            stages,
            access,
            layout,
            reads,
            writes,
            discards: false,
            image_usage: vk::ImageUsageFlags::empty(),
            buffer_usage: vk::BufferUsageFlags::empty(),
        }
    }

    pub(crate) fn color_attachment(load: bool) -> Self {
        let mut access = vk::AccessFlags2::COLOR_ATTACHMENT_WRITE;
        if load {
            access |= vk::AccessFlags2::COLOR_ATTACHMENT_READ;
        }
        Self {
            discards: !load,
            image_usage: vk::ImageUsageFlags::COLOR_ATTACHMENT,
            ..Self::new(
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                access,
                vk::ImageLayout::ATTACHMENT_OPTIMAL,
                load,
                true,
            )
        }
    }

    pub(crate) fn depth_attachment(load: bool, write: bool) -> Self {
        let stages = vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
            | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS;
        let mut access = vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ;
        if write {
            access |= vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE;
        }
        let layout = if write {
            vk::ImageLayout::ATTACHMENT_OPTIMAL
        } else {
            vk::ImageLayout::READ_ONLY_OPTIMAL
        };
        Self {
            discards: write && !load,
            image_usage: vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
            ..Self::new(stages, access, layout, load || !write, write)
        }
    }

    pub(crate) fn of(access: Access) -> Self {
        use vk::AccessFlags2 as A;
        use vk::BufferUsageFlags as B;
        use vk::ImageLayout as L;
        use vk::ImageUsageFlags as I;
        use vk::PipelineStageFlags2 as S;
        let (info, image_usage, buffer_usage) = match access {
            Access::Sampled(s) => (
                Self::new(
                    s.pipeline_stages(),
                    A::SHADER_SAMPLED_READ,
                    L::READ_ONLY_OPTIMAL,
                    true,
                    false,
                ),
                I::SAMPLED,
                B::empty(),
            ),
            Access::StorageRead(s) => (
                Self::new(
                    s.pipeline_stages(),
                    A::SHADER_STORAGE_READ,
                    L::GENERAL,
                    true,
                    false,
                ),
                I::STORAGE,
                B::STORAGE_BUFFER,
            ),
            Access::StorageWrite(s) => (
                Self::new(
                    s.pipeline_stages(),
                    A::SHADER_STORAGE_WRITE,
                    L::GENERAL,
                    false,
                    true,
                ),
                I::STORAGE,
                B::STORAGE_BUFFER,
            ),
            Access::StorageReadWrite(s) => (
                Self::new(
                    s.pipeline_stages(),
                    A::SHADER_STORAGE_READ | A::SHADER_STORAGE_WRITE,
                    L::GENERAL,
                    true,
                    true,
                ),
                I::STORAGE,
                B::STORAGE_BUFFER,
            ),
            Access::Uniform(s) => (
                Self::new(
                    s.pipeline_stages(),
                    A::UNIFORM_READ,
                    L::READ_ONLY_OPTIMAL,
                    true,
                    false,
                ),
                I::empty(),
                B::UNIFORM_BUFFER,
            ),
            Access::TransferRead => (
                Self::new(
                    S::ALL_TRANSFER,
                    A::TRANSFER_READ,
                    L::TRANSFER_SRC_OPTIMAL,
                    true,
                    false,
                ),
                I::TRANSFER_SRC,
                B::TRANSFER_SRC,
            ),
            Access::TransferWrite => (
                Self::new(
                    S::ALL_TRANSFER,
                    A::TRANSFER_WRITE,
                    L::TRANSFER_DST_OPTIMAL,
                    false,
                    true,
                ),
                I::TRANSFER_DST,
                B::TRANSFER_DST,
            ),
            Access::VertexBuffer => (
                Self::new(
                    S::VERTEX_ATTRIBUTE_INPUT,
                    A::VERTEX_ATTRIBUTE_READ,
                    L::UNDEFINED,
                    true,
                    false,
                ),
                I::empty(),
                B::VERTEX_BUFFER,
            ),
            Access::IndexBuffer => (
                Self::new(S::INDEX_INPUT, A::INDEX_READ, L::UNDEFINED, true, false),
                I::empty(),
                B::INDEX_BUFFER,
            ),
            Access::Indirect => (
                Self::new(
                    S::DRAW_INDIRECT,
                    A::INDIRECT_COMMAND_READ,
                    L::UNDEFINED,
                    true,
                    false,
                ),
                I::empty(),
                B::INDIRECT_BUFFER,
            ),
        };
        Self {
            image_usage,
            buffer_usage,
            ..info
        }
    }

    /// Combines two accesses of one resource by one pass. Different layouts fall back
    /// to `GENERAL`.
    pub(crate) fn merge(self, other: Self) -> Self {
        Self {
            stages: self.stages | other.stages,
            access: self.access | other.access,
            layout: if self.layout == other.layout {
                self.layout
            } else {
                vk::ImageLayout::GENERAL
            },
            reads: self.reads || other.reads,
            writes: self.writes || other.writes,
            discards: self.discards && other.discards,
            image_usage: self.image_usage | other.image_usage,
            buffer_usage: self.buffer_usage | other.buffer_usage,
        }
    }
}

/// The state of a texture outside the graph: before an imported texture's first use
/// and after its last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureState {
    /// Layout.
    pub layout: vk::ImageLayout,
    /// Stages that last accessed it (before) or will next access it (after).
    pub stages: vk::PipelineStageFlags2,
    /// The accesses in those stages.
    pub access: vk::AccessFlags2,
}

impl TextureState {
    /// Contents do not matter (a swapchain image right after acquire). The stage is the
    /// one the acquire semaphore is waited on at, so the first barrier chains with it.
    pub const fn acquired_swapchain_image() -> Self {
        Self {
            layout: vk::ImageLayout::UNDEFINED,
            stages: vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            access: vk::AccessFlags2::NONE,
        }
    }

    /// Contents undefined, not in use (a new image before its first write).
    pub const fn undefined() -> Self {
        Self {
            layout: vk::ImageLayout::UNDEFINED,
            stages: vk::PipelineStageFlags2::NONE,
            access: vk::AccessFlags2::NONE,
        }
    }

    /// Ready to present.
    pub const fn present() -> Self {
        Self {
            layout: vk::ImageLayout::PRESENT_SRC_KHR,
            stages: vk::PipelineStageFlags2::NONE,
            access: vk::AccessFlags2::NONE,
        }
    }

    /// Readable by shaders in `stages` (sampled).
    pub fn shader_read(stages: ShaderStages) -> Self {
        Self {
            layout: vk::ImageLayout::READ_ONLY_OPTIMAL,
            stages: stages.pipeline_stages(),
            access: vk::AccessFlags2::SHADER_SAMPLED_READ,
        }
    }

    /// Source of a later copy.
    pub const fn transfer_source() -> Self {
        Self {
            layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            stages: vk::PipelineStageFlags2::ALL_TRANSFER,
            access: vk::AccessFlags2::TRANSFER_READ,
        }
    }
}

/// The state of a buffer outside the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferState {
    /// Stages that last accessed it (before) or will next access it (after).
    pub stages: vk::PipelineStageFlags2,
    /// The accesses in those stages.
    pub access: vk::AccessFlags2,
}

impl BufferState {
    /// Not in use by the GPU (freshly created, or written by the host before submit).
    pub const fn idle() -> Self {
        Self {
            stages: vk::PipelineStageFlags2::NONE,
            access: vk::AccessFlags2::NONE,
        }
    }

    /// Read by the host after the frame's work completes.
    pub const fn host_read() -> Self {
        Self {
            stages: vk::PipelineStageFlags2::HOST,
            access: vk::AccessFlags2::HOST_READ,
        }
    }
}
