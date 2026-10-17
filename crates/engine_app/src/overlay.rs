//! What draws over the player's frame after the game's UI (ADR 0107): the editor's panels.
//! A game draws its UI through `Game::ui`; an overlay adds passes of its own to the frame's
//! graph, over the finished frame, before it is presented ([`crate::Player::frame_overlaid`]).

use crate::AppError;
use engine_render::graph::{FrameGraph, Texture};
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

/// Draws over the player's frame ([`crate::Player::frame_overlaid`]).
pub trait Overlay {
    /// Adds the passes that draw over `pass.target`. What they read must live as long as
    /// the frame's graph: the overlay's own resources, borrowed for `'g`.
    ///
    /// # Errors
    ///
    /// When a pass cannot be declared.
    fn record<'g>(&'g mut self, pass: &mut OverlayPass<'_, 'g>) -> Result<(), AppError>;
}
