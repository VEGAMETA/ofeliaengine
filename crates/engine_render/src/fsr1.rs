//! AMD FidelityFX Super Resolution 1 (ADR 0075): EASU (edge-adaptive spatial upsampling)
//! from the render size to the output size, then RCAS (robust contrast-adaptive
//! sharpening); at the output size RCAS alone. `fsr1.slang` ports the SDK's 32-bit paths
//! (`ffx_fsr1.h`, FidelityFX SDK 1.1.4, MIT; THIRD_PARTY_NOTICES.md); the constants come
//! from `ffxFsrPopulateEasuConstants` and `FsrRcasCon`, ported here.
//!
//! It runs on HDR scene color before bloom and tone mapping (the frame renderer's step
//! 4, ADR 0072), mapped reversibly into 0..1 with the exposure the tone mapper applies,
//! as FSR 1 expects perceptual input.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::graph::{Access, Buffer, FrameGraph, ShaderStages, Texture, TextureDesc};
use crate::post::HDR_FORMAT;
use engine_rhi::{Bindless, Device, RhiError, SamplerKind};

const SPIRV_EASU: &[u8] = include_bytes!("../shaders/fsr1.easu.spv");
const SPIRV_RCAS_SCALED: &[u8] = include_bytes!("../shaders/fsr1.rcasScaled.spv");
const SPIRV_RCAS_NATIVE: &[u8] = include_bytes!("../shaders/fsr1.rcasNative.spv");

/// `Push` in `fsr1.slang` (96 bytes).
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    con: [[u32; 4]; 4],
    source: u32,
    target: u32,
    linear_sampler: u32,
    exposure: f32,
    auto_exposure: u64,
    sharpness: f32,
    has_auto: u32,
}

const _: () = assert!(core::mem::size_of::<Push>() == 96);

/// `ffxFsrPopulateEasuConstants` for an input of `input` pixels (the whole image) and an
/// output of `output`.
fn easu_constants(input: (u32, u32), output: (u32, u32)) -> [[u32; 4]; 4] {
    let (iw, ih) = (input.0 as f32, input.1 as f32);
    let (ow, oh) = (output.0 as f32, output.1 as f32);
    let b = f32::to_bits;
    [
        [
            b(iw / ow),
            b(ih / oh),
            b(0.5 * iw / ow - 0.5),
            b(0.5 * ih / oh - 0.5),
        ],
        [b(1.0 / iw), b(1.0 / ih), b(1.0 / iw), b(-1.0 / ih)],
        [b(-1.0 / iw), b(2.0 / ih), b(1.0 / iw), b(2.0 / ih)],
        [b(0.0), b(4.0 / ih), 0, 0],
    ]
}

/// The exposure the tone mapper will apply: a fixed multiplier, times the automatic
/// exposure's when it is on.
#[derive(Debug, Clone, Copy)]
pub struct Fsr1Exposure {
    /// The fixed part.
    pub value: f32,
    /// The automatic exposure's buffer (`AutoExposure::add_passes`), if on.
    pub auto: Option<Buffer>,
}

/// FSR 1's three compute shaders.
#[derive(Debug)]
pub struct Fsr1 {
    easu: ComputeShader<Push>,
    rcas_scaled: ComputeShader<Push>,
    rcas_native: ComputeShader<Push>,
}

impl Fsr1 {
    /// Creates the pipelines (through the pipeline cache, in parallel).
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        let [easu, rcas_scaled, rcas_native] = ComputeSource::create_all(
            device,
            bindless,
            [
                ComputeSource::new(SPIRV_EASU, c"easu", &[])?,
                ComputeSource::new(SPIRV_RCAS_SCALED, c"rcasScaled", &[])?,
                ComputeSource::new(SPIRV_RCAS_NATIVE, c"rcasNative", &[])?,
            ],
        )?;
        Ok(Self {
            easu,
            rcas_scaled,
            rcas_native,
        })
    }

    /// Declares FSR 1 from `color` (HDR, `render` pixels) to a new HDR image of `output`
    /// pixels: EASU and RCAS, or RCAS alone at the same size. `sharpness` is RCAS's in
    /// stops (0 the strongest, each stop halves it).
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        color: Texture,
        render: (u32, u32),
        output: (u32, u32),
        exposure: Fsr1Exposure,
        sharpness: f32,
    ) -> Texture {
        let result = fg.create_texture("fsr1", TextureDesc::new_2d(HDR_FORMAT, output.0, output.1));
        let base = Push {
            con: easu_constants(render, output),
            source: 0,
            target: 0,
            linear_sampler: Bindless::sampler(SamplerKind::LinearClamp),
            exposure: exposure.value,
            auto_exposure: 0,
            sharpness: (-sharpness.max(0.0)).exp2(),
            has_auto: u32::from(exposure.auto.is_some()),
        };
        let auto = exposure.auto;
        let threads = Dispatch::Threads([output.0, output.1, 1]);
        if render == output {
            fg.add_compute("rcas", bindless)
                .read(color, Access::Sampled(ShaderStages::COMPUTE))
                .maybe_read(auto, Access::StorageRead(ShaderStages::COMPUTE))
                .write(result, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(&self.rcas_native, threads, move |ctx| {
                    Some(Push {
                        source: ctx.sampled_index(color)?,
                        target: ctx.storage_index(result)?,
                        auto_exposure: auto.map_or(0, |b| ctx.address(b)),
                        ..base
                    })
                });
            return result;
        }
        let upscaled =
            fg.create_texture("easu", TextureDesc::new_2d(HDR_FORMAT, output.0, output.1));
        fg.add_compute("easu", bindless)
            .read(color, Access::Sampled(ShaderStages::COMPUTE))
            .maybe_read(auto, Access::StorageRead(ShaderStages::COMPUTE))
            .write(upscaled, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(&self.easu, threads, move |ctx| {
                Some(Push {
                    source: ctx.sampled_index(color)?,
                    target: ctx.storage_index(upscaled)?,
                    auto_exposure: auto.map_or(0, |b| ctx.address(b)),
                    ..base
                })
            });
        fg.add_compute("rcas", bindless)
            .read(upscaled, Access::Sampled(ShaderStages::COMPUTE))
            .maybe_read(auto, Access::StorageRead(ShaderStages::COMPUTE))
            .write(result, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(&self.rcas_scaled, threads, move |ctx| {
                Some(Push {
                    source: ctx.sampled_index(upscaled)?,
                    target: ctx.storage_index(result)?,
                    auto_exposure: auto.map_or(0, |b| ctx.address(b)),
                    ..base
                })
            });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easu_constants_match_the_sdk() {
        // 1280 × 720 → 1920 × 1080, as ffxFsrPopulateEasuConstants computes them.
        let con = easu_constants((1280, 720), (1920, 1080));
        let f = f32::from_bits;
        assert!((f(con[0][0]) - 2.0 / 3.0).abs() < 1e-7);
        assert!((f(con[0][2]) - (0.5 * 2.0 / 3.0 - 0.5)).abs() < 1e-7);
        assert_eq!(f(con[1][3]), -1.0 / 720.0);
        assert_eq!(f(con[2][1]), 2.0 / 720.0);
        assert_eq!(f(con[3][1]), 4.0 / 720.0);
        assert_eq!(con[3][2], 0);
    }
}
