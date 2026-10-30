//! The UI over a view's frame (ADR 0091). The game hands its UI to the frame renderer as
//! canvases instead of drawing them into the frame. The engine can then draw them on
//! every presented frame, real and generated, each with that frame's camera: the HUD is
//! never interpolated, and UI in the world is never smeared.
//!
//! - [`Placement::Screen`]: pixels of the output (a HUD, menus); the same on every frame.
//! - [`Placement::World`]: a canvas in the world (a sign, a panel on a wall), drawn with
//!   the frame's camera and hidden behind what the scene drew in front of it.
//! - [`Placement::Camera`]: attached to the camera (a helmet's visor, a cockpit's screens,
//!   a gun's ammo counter), also depth-tested.
//!
//! UI shown through materials (a monitor's screen texture) is scene content: it is drawn
//! and interpolated with the scene.
//!
//! ```text
//! let frame = renderer.add_view_generating(&mut fg, &mut ctx, &mut view, &scene, &camera,
//!     target, generated, |_, _| {})?;
//! ui.add_passes(&mut fg, ring, &bindless, &layer, target.texture, &camera, &frame)?;
//! if let Some(g) = frame.generated.filter(|g| g.shown) {
//!     ui.add_passes(&mut fg, ring, &bindless, &layer, generated.texture, &g.camera, &frame)?;
//! }
//! ```

use crate::{ViewCamera, ViewFrame};
use engine_core::math::Mat4;
use engine_render::graph::{FrameGraph, Texture};
use engine_render::upload::UploadRing;
use engine_render2d::{Canvas, QuadMode, Render2dError, Renderer2d, SceneDepth, View2d};
use engine_rhi::ash::vk;
use engine_rhi::{Allocator, Bindless, Device, RhiError};

/// Where a canvas of the UI is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    /// Pixels of the output: origin at the top-left corner, y down.
    Screen,
    /// In the world: canvas units (y down, as on screen) to world space.
    World(Mat4),
    /// Attached to the camera: canvas units (y down) to the camera's view space (it looks
    /// down −Z with +Y up).
    Camera(Mat4),
}

/// One canvas of the UI and where it is.
#[derive(Debug, Clone, Copy)]
pub struct UiCanvas<'a> {
    /// The canvas, prepared (`Canvas::prepare`).
    pub canvas: &'a Canvas,
    /// Where it is.
    pub placement: Placement,
}

/// How far behind the scene a canvas in the scene may be and stay visible, in depth units:
/// a panel flush with a wall is not lost to the wall.
const DEPTH_BIAS: f32 = 1e-4;

/// Draws a view's UI layer (see the module documentation).
pub struct UiRenderer {
    renderer: Renderer2d,
}

impl core::fmt::Debug for UiRenderer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UiRenderer").finish_non_exhaustive()
    }
}

impl UiRenderer {
    /// A UI renderer writing `format` (the output's).
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        format: vk::Format,
    ) -> Result<Self, RhiError> {
        Ok(Self {
            renderer: Renderer2d::with_depth_test(
                device,
                allocator,
                bindless,
                format,
                QuadMode::for_device(device),
            )?,
        })
    }

    /// The value white is written as ([`Renderer2d::set_white`]: 1, or SDR white in scRGB
    /// units for an scRGB output).
    pub fn set_white(&mut self, white: f32) {
        self.renderer.set_white(white);
    }

    /// Draws `layer`, in order, over `target` (the output's size) as `camera` sees it: the
    /// real frame's camera, or a generated frame's ([`crate::GeneratedFrame::camera`]).
    /// Canvases in the scene are hidden behind what `frame` drew in front of them.
    #[allow(clippy::too_many_arguments)]
    pub fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        layer: &[UiCanvas<'g>],
        target: Texture,
        camera: &ViewCamera,
        frame: &ViewFrame,
    ) -> Result<(), Render2dError> {
        engine_profiler::zone!("frame::UiRenderer::add_passes");
        let (width, height) = frame.output;
        let aspect = width as f32 / height as f32;
        let depth = SceneDepth {
            texture: frame.depth,
            scale: [
                frame.render.0 as f32 / width as f32,
                frame.render.1 as f32 / height as f32,
            ],
            bias: DEPTH_BIAS,
        };
        for ui in layer {
            match ui.placement {
                Placement::Screen => self.renderer.add_pass(
                    fg,
                    ring,
                    bindless,
                    ui.canvas,
                    &View2d::pixels(width, height),
                    target,
                    None,
                )?,
                Placement::World(world_from_canvas) => self.renderer.add_depth_tested_pass(
                    fg,
                    ring,
                    bindless,
                    ui.canvas,
                    &View2d::from_matrix(
                        camera.clip_from_world(aspect) * world_from_canvas,
                        width,
                        height,
                    ),
                    target,
                    depth,
                )?,
                Placement::Camera(view_from_canvas) => {
                    let projection = camera.clip_from_view(aspect);
                    self.renderer.add_depth_tested_pass(
                        fg,
                        ring,
                        bindless,
                        ui.canvas,
                        &View2d::from_matrix(projection * view_from_canvas, width, height),
                        target,
                        depth,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Frees the renderer's buffers.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses the renderer.
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        // SAFETY: guaranteed by the caller.
        unsafe { self.renderer.destroy(allocator) };
    }
}
