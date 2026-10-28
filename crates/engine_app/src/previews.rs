//! Previews the player draws (ADR 0112's E4-6b): an asset's parts seen alone, framed by
//! their box, in a square image of its own format (RGBA8 sRGB), read back for whoever
//! asked: the editor's thumbnails and the inspector's previews.
//!
//! - **One a frame at most,** once every part is made and each material has its textures.
//! - **A material type's** (ADR 0120's E10-1b) is drawn once to ask for its pipelines and
//!   not answered; it is drawn again, and answered, once no material type's pipeline is on
//!   its way, so that a thumbnail shows the type, not its look.
//! - **A view of the frame renderer** of its own: its draw list, a key light and an
//!   ambient term, no sky, nothing of the scene's; drawn at twice the side and averaged
//!   down (four samples a pixel, edges and shading alike), its alpha kept.
//! - **Framing:** the parts' box seen from the front, turned 22.5° about Y and looking
//!   22.5° down (Godot's previews), as near as the box's corners allow.

use crate::AppError;
use crate::assets::Assets;
use engine_core::math::{Affine3A, Mat4, Quat, Vec3, look_at};
use engine_frame::{
    Antialiasing, Exposure, FrameContext, FrameRenderer, Msaa, PostAntialiasing, PrepareContext,
    SceneDraws, SceneInput, Upscale, View, ViewCamera, ViewSettings, ViewTarget,
};
use engine_render::forward::{DirectionalLight, DrawItem, ViewFeatures};
use engine_render::graph::{Access, BufferState, FrameGraph, ImportedBuffer, LoadOp, TextureDesc};
use engine_render::material::{MaterialFrame, MaterialTable, MaterialTypeId};
use engine_render::mesh::{GeometryFrame, GeometryPool};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, MemoryLocation};
use engine_scene::AssetRef;
use std::collections::VecDeque;

/// A preview's side, in pixels.
pub const PREVIEW_SIZE: u32 = 128;
const FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
/// The side it is drawn at, averaged down 2 × 2.
const DRAWN: u32 = PREVIEW_SIZE * 2;
const BYTES: u64 = (DRAWN * DRAWN * 4) as u64;
/// The previews' camera: a narrow lens, little perspective.
const FOV_Y: f32 = 0.35;

/// A preview to draw: what asks for it (`key`: the editor gives an asset's id) and its
/// parts, each a mesh, its material, and where it is in the preview's space.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewRequest {
    /// Whose preview it is.
    pub key: u64,
    /// The parts.
    pub parts: Vec<(AssetRef, AssetRef, Affine3A)>,
}

/// A preview drawn: its key, and its pixels ([`PREVIEW_SIZE`]², RGBA8 sRGB, rows top to
/// bottom, transparent where nothing is), empty when a part could not be made.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// Whose preview it is.
    pub key: u64,
    /// Its pixels.
    pub pixels: Vec<u8>,
}

struct Readback {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    /// The frame that writes it and the preview's key.
    written: Option<(u64, u64)>,
    /// A material type's first drawing: asked for again once read, not answered.
    again: Option<PreviewRequest>,
}

/// The preview a frame draws: its draws, its camera and its read back's slot (which holds
/// its key).
pub(crate) struct Drawing {
    draws: Vec<DrawItem>,
    camera: ViewCamera,
    slot: usize,
}

/// The player's previews.
pub(crate) struct Previews {
    asked: VecDeque<PreviewRequest>,
    view: Option<View>,
    readbacks: Vec<Readback>,
    done: Vec<Preview>,
    frames_in_flight: u64,
    /// Previews of material types drawn once already (their pipelines asked for), by key.
    warmed: Vec<u64>,
}

impl Previews {
    pub(crate) fn new(frames_in_flight: usize) -> Self {
        Self {
            asked: VecDeque::new(),
            view: None,
            readbacks: Vec::new(),
            done: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            warmed: Vec::new(),
        }
    }

    /// Asks for `request`, in place of one of the same key not drawn yet.
    pub(crate) fn ask(&mut self, request: PreviewRequest) {
        match self.asked.iter_mut().find(|r| r.key == request.key) {
            Some(r) => *r = request,
            None => self.asked.push_back(request),
        }
    }

    /// The previews drawn since the last call.
    pub(crate) fn take(&mut self, out: &mut Vec<Preview>) {
        out.append(&mut self.done);
    }

    /// The previews whose frames finished by `frame`, read back.
    pub(crate) fn collect(&mut self, frame: u64) {
        let completed = frame.saturating_sub(self.frames_in_flight);
        for r in &mut self.readbacks {
            let Some((_, key)) = r.written.filter(|&(f, _)| f <= completed) else {
                continue;
            };
            r.written = None;
            if let Some(request) = r.again.take() {
                if !self.asked.iter().any(|a| a.key == key) {
                    self.asked.push_back(request);
                }
                continue;
            }
            let pixels = r
                .allocation
                .as_ref()
                .and_then(Allocation::mapped_ptr)
                .map(|ptr| {
                    // SAFETY: the frame that wrote it has finished (its graph made the copy
                    // visible to the host); the mapping holds a drawn preview's pixels.
                    let drawn =
                        unsafe { core::slice::from_raw_parts(ptr.as_ptr(), BYTES as usize) };
                    halve(drawn, DRAWN)
                })
                .unwrap_or_default();
            self.done.push(Preview { key, pixels });
        }
    }

    /// The preview this frame draws, if one asked for has every part made (each part's
    /// mesh, and its material with its textures): made now where it was not, its read
    /// back's slot taken. A request with a part that cannot be made is answered with no
    /// pixels. One with a material type waits while `types_pending` (the forward
    /// renderer's material type pipelines on their way) is not zero, after a first drawing
    /// that asks for its pipelines.
    pub(crate) fn pick(
        &mut self,
        assets: &mut Assets,
        geometry: &mut GeometryPool,
        materials: &mut MaterialTable,
        allocator: &mut Allocator,
        frame: u64,
        types_pending: usize,
    ) -> Option<Drawing> {
        if self.asked.is_empty() {
            return None;
        }
        let slot = self.free_slot(allocator)?;
        for _ in 0..self.asked.len() {
            let request = self.asked.pop_front()?;
            match resolve(&request, assets, geometry, materials) {
                Resolved::Ready(draws, (lo, hi)) => {
                    let typed = draws.iter().any(|d| {
                        materials
                            .material_type(d.material)
                            .is_some_and(|t| t != MaterialTypeId::GLTF)
                    });
                    let key = request.key;
                    let warmed = self.warmed.iter().position(|&k| k == key);
                    let again = match (typed, warmed) {
                        (false, _) => None,
                        (true, None) => {
                            self.warmed.push(key);
                            Some(request)
                        }
                        // Its pipelines asked for, some still on their way: later.
                        (true, Some(_)) if types_pending > 0 => {
                            self.asked.push_back(request);
                            continue;
                        }
                        (true, Some(k)) => {
                            self.warmed.swap_remove(k);
                            None
                        }
                    };
                    let readback = &mut self.readbacks[slot];
                    readback.written = Some((frame, key));
                    readback.again = again;
                    return Some(Drawing {
                        draws,
                        camera: framing(lo, hi),
                        slot,
                    });
                }
                Resolved::Waiting => self.asked.push_back(request),
                Resolved::Failed => self.done.push(Preview {
                    key: request.key,
                    pixels: Vec::new(),
                }),
            }
        }
        None
    }

    /// A read back not in flight, made the first time (one more than the frames in
    /// flight).
    fn free_slot(&mut self, allocator: &mut Allocator) -> Option<usize> {
        if self.readbacks.is_empty() {
            for _ in 0..=self.frames_in_flight {
                let (buffer, allocation) = allocator
                    .create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(BYTES)
                            .usage(vk::BufferUsageFlags::TRANSFER_DST),
                        MemoryLocation::Readback,
                    )
                    .ok()?;
                self.readbacks.push(Readback {
                    buffer,
                    allocation: Some(allocation),
                    written: None,
                    again: None,
                });
            }
        }
        self.readbacks.iter().position(|r| r.written.is_none())
    }

    /// The previews' view, made the first time, prepared for this frame.
    pub(crate) fn prepare(
        &mut self,
        renderer: &mut FrameRenderer,
        ctx: PrepareContext<'_>,
    ) -> Result<(), AppError> {
        let view = self.view.get_or_insert_with(|| View::new(ctx.device, 64));
        renderer
            .prepare(view, &settings(), (DRAWN, DRAWN), ctx)
            .map(|_| ())
            .map_err(AppError::at("a preview's view"))
    }

    /// `drawing`'s view and its read back declared into `fg`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn declare<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        ctx: &mut FrameContext<'_, 'g>,
        renderer: &'g FrameRenderer,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        drawing: &'g Drawing,
    ) -> Result<(), AppError> {
        let Some(view) = self.view.as_mut() else {
            return Ok(());
        };
        let image = fg.create_texture("preview", TextureDesc::new_2d(FORMAT, DRAWN, DRAWN));
        let input = SceneInput {
            geometry,
            materials,
            draws: SceneDraws::List(&drawing.draws),
            skinning: None,
            light: DirectionalLight {
                direction: Vec3::new(0.45, -1.0, -0.55).normalize(),
                color: Vec3::splat(2.6),
                ambient: Vec3::splat(0.32),
            },
            lights: &[],
            decals: &[],
            environment: None,
            reflection_probes: None,
            gi: None,
        };
        renderer
            .add_view(
                fg,
                ctx,
                view,
                &input,
                &drawing.camera,
                ViewTarget {
                    texture: image,
                    rect: None,
                    load: LoadOp::DontCare,
                },
            )
            .map_err(AppError::at("a preview"))?;
        let buffer = fg.import_buffer(
            "preview readback",
            ImportedBuffer {
                buffer: self.readbacks[drawing.slot].buffer,
                size: BYTES,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        fg.add_pass("preview copy")
            .read(image, Access::TransferRead)
            .write(buffer, Access::TransferWrite)
            .execute(move |ctx| {
                let region = vk::BufferImageCopy {
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: DRAWN,
                        height: DRAWN,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording outside a rendering scope; the graph put the image in
                // TRANSFER_SRC layout for this pass, and the buffer holds a preview.
                unsafe {
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        ctx.image(image),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.buffer(buffer),
                        &[region],
                    );
                }
            });
        Ok(())
    }

    /// Frees the view and the read backs.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub(crate) unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if let Some(mut view) = self.view.take() {
                view.destroy(allocator, bindless);
            }
            for r in &mut self.readbacks {
                if let Some(allocation) = r.allocation.take() {
                    allocator.destroy_buffer(r.buffer, allocation);
                }
            }
        }
    }
}

/// A preview's view settings: one sample (a resolve would drop the alpha), at its own
/// size, tone mapped into its own format keeping the alpha, nothing temporal, nothing of
/// the scene's effects.
fn settings() -> ViewSettings {
    ViewSettings {
        antialiasing: Antialiasing {
            samples: Msaa::X1,
            upscale: Upscale::Bilinear,
            post: PostAntialiasing::Off,
        },
        exposure: Exposure::Fixed(1.0),
        bloom: false,
        clear: Some([0.0, 0.0, 0.0, 0.0]),
        occlusion_culling: false,
        cached_shadows: false,
        features: ViewFeatures {
            ambient_occlusion: false,
            contact_shadows: false,
            local_shadows: false,
            subsurface: false,
            reflections: false,
            decals: false,
            global_illumination: false,
            depth_prepass: false,
        },
        target_format: Some(FORMAT),
        ..ViewSettings::default()
    }
}

/// `drawn` (`side` × `side`, RGBA8) averaged down 2 × 2: each pixel's alpha the mean of
/// its four, its color theirs weighted by their alpha (no dark fringe at edges).
fn halve(drawn: &[u8], side: u32) -> Vec<u8> {
    let half = (side / 2) as usize;
    let side = side as usize;
    let mut out = vec![0u8; half * half * 4];
    for y in 0..half {
        for x in 0..half {
            let mut color = [0u32; 3];
            let mut alpha = 0u32;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = ((y * 2 + dy) * side + x * 2 + dx) * 4;
                let a = u32::from(drawn[p + 3]);
                for (c, &v) in color.iter_mut().zip(&drawn[p..p + 3]) {
                    *c += u32::from(v) * a;
                }
                alpha += a;
            }
            let o = (y * half + x) * 4;
            if alpha > 0 {
                for (k, c) in color.iter().enumerate() {
                    out[o + k] = ((c + alpha / 2) / alpha).min(255) as u8;
                }
            }
            out[o + 3] = ((alpha + 2) / 4) as u8;
        }
    }
    out
}

enum Resolved {
    /// Every part made: the draws and their box.
    Ready(Vec<DrawItem>, (Vec3, Vec3)),
    Waiting,
    Failed,
}

/// `request`'s parts as draws, once each is made.
fn resolve(
    request: &PreviewRequest,
    assets: &mut Assets,
    geometry: &mut GeometryPool,
    materials: &mut MaterialTable,
) -> Resolved {
    let mut draws = Vec::with_capacity(request.parts.len());
    let mut bounds: Option<(Vec3, Vec3)> = None;
    let mut waiting = false;
    for &(mesh_asset, material_asset, at) in &request.parts {
        let mesh = assets.mesh(mesh_asset, geometry);
        let material = assets.material(material_asset, materials);
        if assets.problem(mesh_asset).is_some() || assets.problem(material_asset).is_some() {
            return Resolved::Failed;
        }
        let (Some(mesh), Some(material)) = (mesh, material) else {
            waiting = true;
            continue;
        };
        if !assets.settled(material) {
            waiting = true;
            continue;
        }
        if let Some(info) = geometry.mesh(mesh) {
            for corner in 0..8 {
                let pick = |bit: usize, axis: usize| {
                    if corner & bit == 0 {
                        info.min[axis]
                    } else {
                        info.max[axis]
                    }
                };
                let p = at.transform_point3(Vec3::new(pick(1, 0), pick(2, 1), pick(4, 2)));
                bounds = Some(bounds.map_or((p, p), |(a, b)| (a.min(p), b.max(p))));
            }
        }
        draws.push(DrawItem {
            mesh,
            material,
            world_from_object: Mat4::from(at),
        });
    }
    match (waiting, bounds) {
        (true, _) => Resolved::Waiting,
        (false, Some(b)) => Resolved::Ready(draws, b),
        (false, None) => Resolved::Failed,
    }
}

/// The camera that frames the box `lo`–`hi`: looking at its middle from the front, turned
/// 22.5° about Y and 22.5° down, as near as its corners allow inside the narrow lens.
fn framing(lo: Vec3, hi: Vec3) -> ViewCamera {
    let center = (lo + hi) * 0.5;
    let turn =
        Quat::from_rotation_y(22.5f32.to_radians()) * Quat::from_rotation_x(-22.5f32.to_radians());
    let back = turn * Vec3::Z;
    let (right, up) = (turn * Vec3::X, turn * Vec3::Y);
    let tan = (FOV_Y * 0.5).tan();
    // Each corner (x across, y up, z toward the camera from the middle) inside the lens:
    // the distance d with |x| / (d - z) ≤ tan and |y| / (d - z) ≤ tan.
    let mut distance = 0.0f32;
    let mut reach = 0.0f32;
    for corner in 0..8 {
        let pick = |bit: usize, a: f32, b: f32| if corner & bit == 0 { a } else { b };
        let p = Vec3::new(
            pick(1, lo.x, hi.x),
            pick(2, lo.y, hi.y),
            pick(4, lo.z, hi.z),
        ) - center;
        let (x, y, z) = (p.dot(right), p.dot(up), p.dot(back));
        distance = distance.max(x.abs().max(y.abs()) / tan + z);
        reach = reach.max(p.length());
    }
    // A little room around it; something there however small.
    let distance = (distance * 1.08).max(reach.max(1e-3) * 1.5);
    let eye = center + back * distance;
    ViewCamera {
        view_from_world: look_at(eye, center, Vec3::Y),
        position: eye,
        fov_y: FOV_Y,
        near: (distance - reach * 1.5).max(distance * 0.01),
        far: distance + reach * 1.5,
        cut: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_averages_by_alpha() {
        // A 2 × 2 block: two red opaque, two transparent black: half-covered red.
        let drawn = [255, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 0, 0, 0, 0];
        assert_eq!(halve(&drawn, 2), [255, 0, 0, 128]);
        assert_eq!(halve(&[0; 16], 2), [0, 0, 0, 0]);
    }

    #[test]
    fn a_box_is_framed_from_the_front_right_and_above_its_corners_inside() {
        let (lo, hi) = (Vec3::new(-1.0, 0.0, -0.5), Vec3::new(1.0, 2.0, 0.5));
        let camera = framing(lo, hi);
        let center = (lo + hi) * 0.5;
        // From the front (+Z), the right (+X) and above.
        let from = camera.position - center;
        assert!(from.z > 0.0 && from.x > 0.0 && from.y > 0.0, "{from}");
        // Every corner inside the lens, and the nearest one past the near plane.
        let clip = camera.clip_from_world(1.0);
        for corner in 0..8 {
            let pick = |bit: usize, a: f32, b: f32| if corner & bit == 0 { a } else { b };
            let p = Vec3::new(
                pick(1, lo.x, hi.x),
                pick(2, lo.y, hi.y),
                pick(4, lo.z, hi.z),
            );
            let c = clip * p.extend(1.0);
            let (x, y, z) = (c.x / c.w, c.y / c.w, c.z / c.w);
            assert!(
                x.abs() <= 1.0 && y.abs() <= 1.0,
                "corner {corner} at {x}, {y}"
            );
            assert!((0.0..=1.0).contains(&z), "corner {corner} depth {z}");
        }
    }
}
