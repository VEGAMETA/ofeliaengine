//! Compute passes with little ceremony (ADR 0052): a [`ComputeShader`] is a pipeline tied
//! to the Rust type of its push constants, checked against the shader's SPIR-V when it is
//! created; [`FrameGraph::add_compute`] declares a pass that binds shaders, pushes their
//! constants and dispatches enough workgroups for a number of threads. Pass code has no
//! `unsafe`.
//!
//! ```text
//! // Rust: the push constants, as the shader declares them.
//! #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
//! #[repr(C)]
//! struct Push { source: u32, target: u32, size: [u32; 2], strength: f32, _pad: [u32; 3] }
//!
//! let blur = ComputeShader::<Push>::new(&device, &bindless, BLUR_SPV, c"blur")?;
//!
//! // Each frame, one dispatch:
//! fg.add_compute("blur", &bindless)
//!     .read(source, Access::Sampled(ShaderStages::COMPUTE))
//!     .write(target, Access::StorageWrite(ShaderStages::COMPUTE))
//!     .dispatch(&blur, Dispatch::Threads([width, height, 1]), move |ctx| {
//!         Some(Push {
//!             source: ctx.sampled_index(source)?,
//!             target: ctx.storage_index(target)?,
//!             size: [width, height],
//!             strength: 0.5,
//!             _pad: [0; 3],
//!         })
//!     });
//!
//! // Several dispatches in one pass (a mip chain of one image):
//! fg.add_compute("mips", &bindless)
//!     .write(image, Access::StorageReadWrite(ShaderStages::COMPUTE))
//!     .record(move |rec| {
//!         for level in 1..levels {
//!             rec.dispatch(&reduce, Dispatch::Threads([w >> level, h >> level, 1]), &push(level));
//!             rec.barrier(); // the next level reads this one
//!         }
//!     });
//! ```

use crate::graph::{Access, FrameGraph, GraphResource, PassBuilder, PassContext};
use core::ffi::CStr;
use core::marker::PhantomData;
use engine_rhi::ash::vk;
use engine_rhi::spirv::reflect;
use engine_rhi::{
    Bindless, Device, PUSH_CONSTANT_BYTES, Pipeline, RhiError, ShaderStageDesc, spirv_words,
};

/// A compute pipeline whose push constants are a `P`.
pub struct ComputeShader<P> {
    pipeline: Pipeline,
    local_size: [u32; 3],
    _push: PhantomData<fn(P)>,
}

impl<P> core::fmt::Debug for ComputeShader<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComputeShader")
            .field("local_size", &self.local_size)
            .finish_non_exhaustive()
    }
}

impl<P: bytemuck::Pod> ComputeShader<P> {
    /// Creates the pipeline of `entry` in `spirv` (the bytes of a `.spv` file, usually
    /// `include_bytes!`), with `bindless`'s layout. Fails if the shader reads more push
    /// constants than a `P` holds.
    pub fn new(
        device: &Device,
        bindless: &Bindless,
        spirv: &[u8],
        entry: &CStr,
    ) -> Result<Self, RhiError> {
        Self::specialized(device, bindless, spirv, entry, &[])
    }

    /// Like [`Self::new`], with specialization constants (`specialization[i]` is constant
    /// id `i`). The workgroup size is the module's own; specializing it is not supported.
    pub fn specialized(
        device: &Device,
        bindless: &Bindless,
        spirv: &[u8],
        entry: &CStr,
        specialization: &[u32],
    ) -> Result<Self, RhiError> {
        const {
            assert!(
                core::mem::size_of::<P>() <= PUSH_CONSTANT_BYTES as usize,
                "push constants are limited to PUSH_CONSTANT_BYTES"
            );
        }
        let local_size = check_interface::<P>(spirv, entry)?;
        let words = spirv_words(spirv)
            .ok_or_else(|| RhiError::ShaderInterface(format!("{entry:?}: not SPIR-V")))?;
        let pipeline = Pipeline::compute_specialized(
            device,
            bindless.pipeline_layout(),
            ShaderStageDesc {
                spirv: &words,
                entry,
            },
            specialization,
            vk::PipelineCache::null(),
        )?;
        Ok(Self {
            pipeline,
            local_size,
            _push: PhantomData,
        })
    }

    /// Invocations per workgroup (`numthreads`).
    pub fn local_size(&self) -> [u32; 3] {
        self.local_size
    }

    /// The pipeline, for recording by hand.
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// Workgroup counts for `size`.
    pub fn groups(&self, size: Dispatch) -> [u32; 3] {
        match size {
            Dispatch::Groups(groups) => groups,
            Dispatch::Threads(threads) => {
                [0, 1, 2].map(|i| threads[i].div_ceil(self.local_size[i].max(1)))
            }
        }
    }
}

/// Checks that a `P` covers the push constants of the compute shader in `spirv` and
/// returns its workgroup size (what [`ComputeShader::new`] checks, without a device).
pub fn check_interface<P>(spirv: &[u8], entry: &CStr) -> Result<[u32; 3], RhiError> {
    let fail = |what: String| RhiError::ShaderInterface(format!("{entry:?}: {what}"));
    let words = spirv_words(spirv).ok_or_else(|| fail("not SPIR-V".into()))?;
    let interface =
        reflect(&words).ok_or_else(|| fail("SPIR-V this reader cannot follow".into()))?;
    let local_size = interface
        .local_size
        .ok_or_else(|| fail("not a compute shader (no workgroup size)".into()))?;
    let (needed, given) = (
        interface.push_constant_bytes as usize,
        core::mem::size_of::<P>(),
    );
    if needed > given {
        return Err(fail(format!(
            "reads {needed} bytes of push constants, but {} has {given}",
            core::any::type_name::<P>()
        )));
    }
    Ok(local_size)
}

/// How much to dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// At least this many threads along x, y and z: the workgroup counts are rounded up
    /// from the shader's workgroup size (the shader checks its bounds).
    Threads([u32; 3]),
    /// Exactly these workgroup counts.
    Groups([u32; 3]),
}

/// A compute pass being declared (see [`FrameGraph::add_compute`]).
pub struct ComputePass<'f, 'g> {
    pass: PassBuilder<'f, 'g>,
    bindless: &'g Bindless,
}

impl core::fmt::Debug for ComputePass<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComputePass").finish_non_exhaustive()
    }
}

impl<'g> FrameGraph<'g> {
    /// Declares a compute pass whose shaders use `bindless`: declare what it reads and
    /// writes, then [`ComputePass::dispatch`] one shader or [`ComputePass::record`]
    /// several dispatches.
    pub fn add_compute<'f>(
        &'f mut self,
        name: &'static str,
        bindless: &'g Bindless,
    ) -> ComputePass<'f, 'g> {
        ComputePass {
            pass: self.add_pass(name),
            bindless,
        }
    }
}

impl<'g> ComputePass<'_, 'g> {
    /// Declares a read (as [`PassBuilder::read`]).
    pub fn read(mut self, resource: impl GraphResource, access: Access) -> Self {
        self.pass = self.pass.read(resource, access);
        self
    }

    /// Declares a write (as [`PassBuilder::write`]).
    pub fn write(mut self, resource: impl GraphResource, access: Access) -> Self {
        self.pass = self.pass.write(resource, access);
        self
    }

    /// Keeps the pass even if nothing reads what it writes (as
    /// [`PassBuilder::side_effects`]).
    pub fn side_effects(mut self) -> Self {
        self.pass = self.pass.side_effects();
        self
    }

    /// Dispatches `shader` over `size` with the push constants `push` builds from the pass
    /// context (bindless indices, buffer addresses). `push` returning `None` (a resource
    /// without its bindless index) skips the dispatch.
    pub fn dispatch<P: bytemuck::Pod>(
        self,
        shader: &'g ComputeShader<P>,
        size: Dispatch,
        push: impl FnOnce(&PassContext<'_>) -> Option<P> + 'g,
    ) {
        self.record(move |rec| {
            if let Some(push) = push(rec.context()) {
                rec.dispatch(shader, size, &push);
            }
        });
    }

    /// Records any number of dispatches, of any shaders, in this pass.
    pub fn record(self, record: impl FnOnce(&mut ComputeRecorder<'_, '_>) + 'g) {
        let bindless = self.bindless;
        self.pass.execute(move |ctx| {
            let mut recorder = ComputeRecorder {
                ctx,
                bindless,
                bound: false,
            };
            record(&mut recorder);
        });
    }
}

/// Records dispatches inside a compute pass (see [`ComputePass::record`]).
pub struct ComputeRecorder<'r, 'x> {
    ctx: &'r PassContext<'x>,
    bindless: &'r Bindless,
    bound: bool,
}

impl core::fmt::Debug for ComputeRecorder<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComputeRecorder").finish_non_exhaustive()
    }
}

impl<'x> ComputeRecorder<'_, 'x> {
    /// The pass context: bindless indices and buffer addresses of the pass's resources.
    pub fn context(&self) -> &PassContext<'x> {
        self.ctx
    }

    /// Dispatches `shader` over `size` with `push`.
    pub fn dispatch<P: bytemuck::Pod>(
        &mut self,
        shader: &ComputeShader<P>,
        size: Dispatch,
        push: &P,
    ) {
        let groups = shader.groups(size);
        if groups.contains(&0) {
            return;
        }
        let ctx = self.ctx;
        // SAFETY: a compute pass has no attachments, so this records outside a rendering
        // scope; the pipeline and the bindless set outlive the frame, and `P` fits the
        // push constant range (checked when the shader was created).
        unsafe {
            shader.pipeline.bind(ctx.cmd);
            if !self.bound {
                self.bindless.bind(ctx.cmd, vk::PipelineBindPoint::COMPUTE);
                self.bound = true;
            }
            self.bindless
                .push_constants(ctx.cmd, bytemuck::bytes_of(push));
            ctx.device
                .cmd_dispatch(ctx.cmd, groups[0], groups[1], groups[2]);
        }
    }

    /// Makes the storage writes of the dispatches so far visible to the ones after (for
    /// passes that feed one dispatch into the next, as a mip chain does).
    pub fn barrier(&mut self) {
        let barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .dst_access_mask(
                vk::AccessFlags2::SHADER_STORAGE_READ
                    | vk::AccessFlags2::SHADER_STORAGE_WRITE
                    | vk::AccessFlags2::SHADER_SAMPLED_READ,
            );
        // SAFETY: recording outside a rendering scope (a compute pass).
        unsafe {
            self.ctx.device.cmd_pipeline_barrier2(
                self.ctx.cmd,
                &vk::DependencyInfo::default().memory_barriers(&[barrier]),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct Small([u32; 4]);

    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    #[repr(C)]
    struct Big([u32; 24]);

    #[test]
    fn push_constants_smaller_than_the_shader_reads_are_refused() {
        let spirv = include_bytes!("../shaders/postfx.bloomDownsample.spv");
        assert_eq!(
            check_interface::<Big>(spirv, c"bloomDownsample").ok(),
            Some([8, 8, 1])
        );
        let error = check_interface::<Small>(spirv, c"bloomDownsample")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("reads 96 bytes"), "{error}");
        assert!(check_interface::<Big>(b"not spirv", c"x").is_err());
        let graphics = include_bytes!("../shaders/tonemap.fragmentMain.spv");
        assert!(check_interface::<Big>(graphics, c"fragmentMain").is_err());
    }

    #[test]
    fn every_compute_shader_matches_its_push_constants() {
        // The engine's own compute shaders against the Rust types that push to them.
        let shaders: [(&[u8], usize); 11] = [
            (include_bytes!("../shaders/postfx.bloomDownsample.spv"), 96),
            (include_bytes!("../shaders/postfx.bloomUpsample.spv"), 96),
            (
                include_bytes!("../shaders/postfx.exposureHistogram.spv"),
                96,
            ),
            (include_bytes!("../shaders/postfx.exposureAverage.spv"), 96),
            (include_bytes!("../shaders/environment.skyCube.spv"), 96),
            (include_bytes!("../shaders/environment.downsample.spv"), 96),
            (include_bytes!("../shaders/environment.prefilter.spv"), 96),
            (include_bytes!("../shaders/environment.irradiance.spv"), 96),
            (include_bytes!("../shaders/environment.dfg.spv"), 96),
            (include_bytes!("../shaders/hiz.hizReduce.spv"), 64),
            (include_bytes!("../shaders/forward.cullMain.spv"), 112),
        ];
        for (spirv, bytes) in shaders {
            let words = spirv_words(spirv).unwrap_or_default();
            let interface = reflect(&words);
            assert_eq!(
                interface.map(|i| i.push_constant_bytes as usize),
                Some(bytes)
            );
        }
    }
}
