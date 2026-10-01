//! Contact shadows of the sun (ADR 0071, docs/research/local-shadows.md): what the depth
//! buffer hides from the sun at scales its cascades cannot resolve, as a visibility mask
//! the forward pass multiplies into the sun's light. Bend Studio's screen-space shadows
//! (Days Gone; `bend_sss`, Copyright 2023 Sony Interactive Entertainment, Apache-2.0,
//! adapted; see THIRD_PARTY_NOTICES.md): lines of 64 pixels towards the light's point on
//! screen, their depths shared between the threads; 60 samples per pixel for a few reads.
//! See `contact.slang`.
//!
//! The forward renderer runs it after its depth prepass when
//! [`crate::forward::ForwardRenderer::set_contact_shadows`] turns it on.

use crate::compute::{ComputeShader, Dispatch};
use crate::graph::{Access, FrameGraph, ShaderStages, Texture, TextureDesc};
use engine_core::math::{Mat4, Vec3, Vec4};
use engine_rhi::ash::vk;
use engine_rhi::{Bindless, Device, RhiError};

const BEND: &[u8] = include_bytes!("../shaders/contact.contactBend.spv");

/// Format of the visibility mask: 1 lit, 0 in a contact shadow.
pub const CONTACT_FORMAT: vk::Format = vk::Format::R8_UNORM;

/// Threads of a Bend wave (`WAVE_SIZE` in `contact.slang`).
const WAVE: i32 = 64;

/// Contact shadows' settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactShadowSettings {
    /// A pixel's assumed thickness, as a part of the depth between it and the far
    /// plane (0.01; Bend's 0.005 leaves solid casters' shadows faint with a conventional depth buffer, ADR 0071).
    pub surface_thickness: f32,
    /// A depth step, as such a part, beyond which two neighbours are not interpolated
    /// (0.02).
    pub bilinear_threshold: f32,
    /// Contrast of the transition into shadow (4).
    pub contrast: f32,
}

impl Default for ContactShadowSettings {
    fn default() -> Self {
        Self {
            surface_thickness: 0.01,
            bilinear_threshold: 0.02,
            contrast: 4.0,
        }
    }
}

/// `ContactPush` in `contact.slang` (64 bytes).
#[derive(Debug, Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    light: [f32; 4],
    wave_offset: [i32; 2],
    size: [u32; 2],
    surface_thickness: f32,
    bilinear_threshold: f32,
    shadow_contrast: f32,
    far_depth: f32,
    near_depth: f32,
    depth: u32,
    target: u32,
    /// The first dispatch writes the light's own pixel (lit), which no line covers.
    light_pixel: u32,
}

const _: () = assert!(size_of::<Push>() == 64);

/// The camera of a frame, as contact shadows need it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ContactView {
    /// Clip from world as the depth was drawn (jittered).
    pub clip_from_world: Mat4,
    /// Unit vector towards the sun.
    pub to_light: Vec3,
}

/// One dispatch of the Bend method: its wave counts and first wave.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BendDispatch {
    pub waves: [u32; 3],
    pub offset: [i32; 2],
}

/// The light's point on screen and the dispatches covering a `size` target (Bend's
/// `BuildDispatchList`, for a Vulkan projection: clip y points down).
pub(crate) fn bend_dispatches(light: Vec4, size: [u32; 2]) -> ([f32; 4], [BendDispatch; 8], usize) {
    let (width, height) = (size[0] as f32, size[1] as f32);
    // Precision limit of the division when the light is very far off screen.
    let limit = 0.000_002 * WAVE as f32;
    let mut w = light.w;
    if w >= 0.0 && w < limit {
        w = limit;
    } else if w < 0.0 && w > -limit {
        w = -limit;
    }
    let coordinate = [
        (light.x / w * 0.5 + 0.5) * width,
        (light.y / w * 0.5 + 0.5) * height,
        if light.w == 0.0 {
            0.0
        } else {
            light.z / light.w
        },
        if light.w > 0.0 { 1.0 } else { -1.0 },
    ];
    let light_xy = [
        (coordinate[0] + 0.5).floor() as i32,
        (coordinate[1] + 0.5).floor() as i32,
    ];
    let (min, max) = ([0i32, 0i32], [size[0] as i32, size[1] as i32]);
    // Bounds relative to the light (y up, as in Bend's code).
    let biased = [
        min[0] - light_xy[0],
        -(max[1] - light_xy[1]),
        max[0] - light_xy[0],
        -(min[1] - light_xy[1]),
    ];
    let mut out = [BendDispatch::default(); 8];
    let mut count = 0usize;
    // Wave counts as signed integers while splitting.
    let mut list = [([0i32; 3], [0i32; 2]); 8];
    for q in 0..4i32 {
        let vertical = q == 0 || q == 3;
        let pick = |a: bool, x: i32, y: i32| if a { x } else { y };
        let bounds = [
            pick(q & 1 != 0, biased[0], -biased[2]).max(0) / WAVE,
            pick(q & 2 != 0, biased[1], -biased[3]).max(0) / WAVE,
            (pick(q & 1 != 0, biased[2], -biased[0]) + WAVE * if vertical { 1 } else { 2 } - 1)
                .max(0)
                / WAVE,
            (pick(q & 2 != 0, biased[3], -biased[1]) + WAVE * if vertical { 2 } else { 1 } - 1)
                .max(0)
                / WAVE,
        ];
        if bounds[2] - bounds[0] <= 0 || bounds[3] - bounds[1] <= 0 {
            continue;
        }
        let bias_x = i32::from(q == 2 || q == 3);
        let bias_y = i32::from(q == 1 || q == 3);
        let mut d = (
            [WAVE, bounds[2] - bounds[0], bounds[3] - bounds[1]],
            [
                pick(q & 1 != 0, bounds[0], -bounds[2]) + bias_x,
                pick(q & 2 != 0, -bounds[3], bounds[1]) + bias_y,
            ],
        );
        // The far corner of the quadrant: where the diagonal from the light leaves it.
        let mut axis_delta = biased[0] - biased[1];
        if q == 1 {
            axis_delta = biased[2] + biased[1];
        }
        if q == 2 {
            axis_delta = -biased[0] - biased[3];
        }
        if q == 3 {
            axis_delta = -biased[2] + biased[3];
        }
        axis_delta = (axis_delta + WAVE - 1) / WAVE;
        let mut second = None;
        if axis_delta > 0 {
            let mut d2 = d;
            match q {
                0 => {
                    d2.0[2] = d.0[2].min(axis_delta);
                    d.0[2] -= d2.0[2];
                    d2.1[1] = d.1[1] + d.0[2];
                    d2.1[0] -= 1;
                    d2.0[1] += 1;
                }
                1 => {
                    d2.0[1] = d.0[1].min(axis_delta);
                    d.0[1] -= d2.0[1];
                    d2.1[0] = d.1[0] + d.0[1];
                    d2.0[2] += 1;
                }
                2 => {
                    d2.0[1] = d.0[1].min(axis_delta);
                    d.0[1] -= d2.0[1];
                    d.1[0] += d2.0[1];
                    d2.0[2] += 1;
                    d2.1[1] -= 1;
                }
                _ => {
                    d2.0[2] = d.0[2].min(axis_delta);
                    d.0[2] -= d2.0[2];
                    d.1[1] += d2.0[2];
                    d2.0[1] += 1;
                }
            }
            if d2.0[1] > 0 && d2.0[2] > 0 {
                second = Some(d2);
            }
        }
        if d.0[1] > 0 && d.0[2] > 0 {
            list[count] = d;
            count += 1;
        }
        if let Some(d2) = second {
            list[count] = d2;
            count += 1;
        }
    }
    for (out, (waves, offset)) in out.iter_mut().zip(&list[..count]) {
        *out = BendDispatch {
            waves: [waves[0] as u32, waves[1] as u32, waves[2] as u32],
            offset: [offset[0] * WAVE, offset[1] * WAVE],
        };
    }
    (coordinate, out, count)
}

/// The contact shadow passes.
pub struct ContactShadows {
    bend: ComputeShader<Push>,
}

impl core::fmt::Debug for ContactShadows {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ContactShadows").finish_non_exhaustive()
    }
}

impl ContactShadows {
    /// Creates the pipelines.
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        Ok(Self {
            bend: ComputeShader::new(device, bindless, BEND, c"contactBend")?,
        })
    }

    /// Adds the pass computing the sun's contact shadows from `depth` (sampled) seen by
    /// `view`, and returns the visibility ([`CONTACT_FORMAT`], the depth's size).
    pub(crate) fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        depth: Texture,
        view: &ContactView,
        settings: &ContactShadowSettings,
    ) -> Texture {
        let desc = fg.texture_desc(depth);
        let size = [desc.width.max(1), desc.height.max(1)];
        let mask = fg.create_texture(
            "contact shadows",
            TextureDesc::new_2d(CONTACT_FORMAT, size[0], size[1]),
        );
        let base = Push {
            size,
            surface_thickness: settings.surface_thickness.max(1e-6),
            bilinear_threshold: settings.bilinear_threshold,
            shadow_contrast: settings.contrast.max(1.0),
            far_depth: 1.0,
            near_depth: 0.0,
            ..Push::default()
        };
        let light = view.clip_from_world * view.to_light.extend(0.0);
        let (coordinate, dispatches, count) = bend_dispatches(light, size);
        let bend = &self.bend;
        // The lines meet at the light's point (in front of the camera or behind):
        // when it is on screen, its own pixel may be left unwritten. Lit there.
        let on_screen = (0.0..size[0] as f32).contains(&coordinate[0])
            && (0.0..size[1] as f32).contains(&coordinate[1]);
        // The dispatches cover every pixel once (pixels without geometry are
        // written lit).
        mask_pass(fg, bindless, depth, mask).record(move |rec| {
            let ctx = rec.context();
            let (Some(depth), Some(target)) = (ctx.sampled_index(depth), ctx.storage_index(mask))
            else {
                return;
            };
            for (k, d) in dispatches[..count].iter().enumerate() {
                let push = Push {
                    light: coordinate,
                    wave_offset: d.offset,
                    depth,
                    target,
                    light_pixel: u32::from(on_screen && k == 0),
                    ..base
                };
                rec.dispatch(bend, Dispatch::Groups(d.waves), &push);
            }
        });
        mask
    }
}

/// The pass writing the mask from the depth.
fn mask_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    bindless: &'g Bindless,
    depth: Texture,
    mask: Texture,
) -> crate::compute::ComputePass<'a, 'g> {
    // On the async compute queue where that is the default (ADR 0065), as ambient
    // occlusion.
    fg.add_compute("contact shadows", bindless)
        .async_compute()
        .read(depth, Access::Sampled(ShaderStages::COMPUTE))
        .write(mask, Access::StorageWrite(ShaderStages::COMPUTE))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pixel of the target lies in exactly one dispatch's waves (Bend's layout):
    /// pixel positions of all threads, as `wavefront_extents` computes them, rounded.
    fn covered(light: Vec4, size: [u32; 2]) -> Vec<u32> {
        let (coordinate, dispatches, count) = bend_dispatches(light, size);
        let mut hits = vec![0u32; (size[0] * size[1]) as usize];
        let light_xy = [coordinate[0].floor() + 0.5, coordinate[1].floor() + 0.5];
        let fraction = [coordinate[0] - light_xy[0], coordinate[1] - light_xy[1]];
        for d in &dispatches[..count] {
            for gz in 0..d.waves[2] as i32 {
                for gy in 0..d.waves[1] as i32 {
                    for gx in 0..d.waves[0] as i32 {
                        let mut xy = [gy * WAVE + d.offset[0], gz * WAVE + d.offset[1]];
                        let sign = [xy[0].signum(), xy[1].signum()];
                        let horizontal = (xy[0] + sign[1]).abs() < (xy[1] - sign[0]).abs();
                        let axis = if horizontal {
                            [sign[1], 0]
                        } else {
                            [0, -sign[0]]
                        };
                        xy = [axis[0] * gx + xy[0], axis[1] * gx + xy[1]];
                        let xf = [xy[0] as f32, xy[1] as f32];
                        let x_major = xf[0].abs() > xf[1].abs();
                        let major = if x_major { xf[0] } else { xf[1] };
                        let start = major.abs();
                        let end = start - WAVE as f32;
                        let mut frac = if x_major { fraction[0] } else { fraction[1] };
                        frac = if major > 0.0 { -frac } else { frac };
                        let s = [xf[0] + light_xy[0], xf[1] + light_xy[1]];
                        let t = (end + frac) / (start + frac);
                        let e = [
                            coordinate[0] + (s[0] - coordinate[0]) * t,
                            coordinate[1] + (s[1] - coordinate[1]) * t,
                        ];
                        for thread in 0..WAVE {
                            let k = thread as f32 / WAVE as f32;
                            let p = [s[0] + (e[0] - s[0]) * k, s[1] + (e[1] - s[1]) * k];
                            let (x, y) = (p[0].floor() as i32, p[1].floor() as i32);
                            if x >= 0 && y >= 0 && (x as u32) < size[0] && (y as u32) < size[1] {
                                hits[(y as u32 * size[0] + x as u32) as usize] += 1;
                            }
                        }
                    }
                }
            }
        }
        hits
    }

    #[test]
    fn bend_dispatches_cover_every_pixel() {
        let size = [320, 180];
        // The light on screen, off screen to the side, behind the camera.
        for light in [
            Vec4::new(0.2, -0.3, 0.5, 1.0),
            Vec4::new(3.0, 0.4, 0.5, 1.0),
            Vec4::new(-0.5, 0.1, 0.5, -1.0),
            Vec4::new(0.0, 5.0, 0.2, 0.01),
        ] {
            let hits = covered(light, size);
            // All but the light's own pixel (where every line ends; `add_passes` clears
            // the mask first when it is on screen).
            let (coordinate, _, _) = bend_dispatches(light, size);
            let own = ((0.0..size[0] as f32).contains(&coordinate[0])
                && (0.0..size[1] as f32).contains(&coordinate[1]))
            .then(|| (coordinate[1] as u32 * size[0] + coordinate[0] as u32) as usize);
            let missing: Vec<usize> = hits
                .iter()
                .enumerate()
                .filter(|&(i, &h)| h == 0 && Some(i) != own)
                .map(|(i, _)| i)
                .collect();
            assert!(missing.is_empty(), "{light:?}: {missing:?} not covered");
        }
    }
}
