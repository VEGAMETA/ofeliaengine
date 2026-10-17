//! What draws over the player's frame after the game's UI (ADR 0107): the editor's panels.
//! A game draws its UI through `Game::ui`; an overlay adds passes of its own to the frame's
//! graph, over the finished frame, before it is presented ([`crate::Player::frame_overlaid`]).

use crate::{AppError, Setup};
use engine_render::graph::{FrameGraph, Texture};
use engine_render::post::OutputLevels;
use engine_render::upload::UploadRing;
use engine_rhi::Bindless;
use engine_rhi::ash::vk;

/// The frame an overlay draws over.
pub struct OverlayPass<'a, 'g> {
    /// The frame's graph.
    pub fg: &'a mut FrameGraph<'g>,
    /// The frame's upload ring.
    pub ring: &'a mut UploadRing,
    /// The bindless set the frame executes with.
    pub bindless: &'g Bindless,
    /// The finished frame: the scene (in its rectangle, `Player::set_view_rect`) and the
    /// game's UI.
    pub target: Texture,
    /// Its size in pixels.
    pub extent: vk::Extent2D,
}

/// What an overlay draws into: [`crate::Player::output`], and given again to
/// [`Overlay::output_changed`] when it changes (HDR turned on or off, ADR 0038).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayOutput {
    /// The format of [`OverlayPass::target`]: the swapchain's, the offscreen image's, or
    /// HDR10's linear intermediate.
    pub format: vk::Format,
    /// The value UI white is written as (`Renderer2d::set_white`): 1 in SDR and in HDR10's
    /// intermediate, the SDR white level in scRGB units (nits / 80) for scRGB.
    pub white: f32,
    /// How the output reaches the display.
    pub levels: OutputLevels,
}

/// Draws over the player's frame ([`crate::Player::frame_overlaid`]).
pub trait Overlay {
    /// Adds the passes that draw over `pass.target`. What they read must live as long as
    /// the frame's graph: the overlay's own resources, borrowed for `'g`.
    ///
    /// # Errors
    ///
    /// When a pass cannot be declared.
    fn record<'g>(&'g mut self, pass: &mut OverlayPass<'_, 'g>) -> Result<(), AppError>;

    /// The target's format or white level changed: pipelines made for the old format are
    /// made again, the white level set. Called with the device idle, before the next
    /// [`Self::record`]. Nothing by default.
    ///
    /// # Errors
    ///
    /// When the overlay's pipelines cannot be made again.
    fn output_changed(
        &mut self,
        setup: &mut Setup<'_>,
        output: &OverlayOutput,
    ) -> Result<(), AppError> {
        let _ = (setup, output);
        Ok(())
    }
}
