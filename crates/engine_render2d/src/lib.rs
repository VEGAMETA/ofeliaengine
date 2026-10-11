//! 2D rendering (CLAUDE.md §6 "2D renderer"): sprites and SDF shapes, batched through
//! bindless textures, on the render graph (ADR 0015).
//!
//! - [`Canvas`]: a frame's draws. [`Quad`] covers sprites (textured or solid, with hard
//!   pixel edges) and antialiased SDF shapes: rectangles with per-corner radii, circles,
//!   capsule lines, and their outlines.
//! - Draw order is deterministic: [`Order`] layer, then a per-layer key (for example
//!   y-sorting), then submission order.
//! - Batches split only on blend pipeline ([`Blend2d::Multiply`] vs the rest) and clip
//!   rectangle. Texture changes do not split them: every quad carries its bindless index.
//! - [`Renderer2d`] copies the quads into the upload ring (64 bytes each, plus a 4-byte
//!   draw order when sorting moved anything) and draws each batch with one draw; vertices
//!   are generated from the vertex index (no vertex buffers), indexed or as a plain list
//!   depending on the GPU vendor ([`QuadMode::for_vendor`]).
//! - [`Camera2d`] and [`View2d`]: a y-up world camera and y-down pixel space.
//! - Text: [`GlyphTexture`] keeps an `engine_text` glyph atlas on the GPU, and
//!   [`draw_text`] turns a laid-out text into glyph quads (pixel space); [`draw_glyphs`]
//!   does the same for glyphs kept from an earlier layout.
//!
//! ```no_run
//! # use engine_render2d::*;
//! # use engine_core::math::{Vec2, Vec4};
//! let mut canvas = Canvas::new();
//! // Every frame:
//! canvas.clear();
//! canvas.draw(&Quad::sprite(7, Vec2::new(0.0, 0.0), Vec2::new(2.0, 2.0)).order(Order::y_sorted(0, -1.0)));
//! canvas.draw(&Quad::circle(Vec2::new(3.0, 1.0), 0.5).color(Vec4::new(1.0, 0.2, 0.2, 1.0)));
//! canvas.draw(&Quad::line(Vec2::ZERO, Vec2::new(4.0, 3.0), 0.1).blend(Blend2d::Additive));
//! canvas.prepare(); // sort and batch: on any thread
//! // renderer.add_pass(&mut fg, &mut ring, &bindless, &canvas, &camera.view(w, h), target, clear)?;
//! ```

mod canvas;
mod renderer;
mod sort;
mod text;
mod view;

pub use canvas::{Blend2d, Canvas, CanvasStats, ClipRect, MAX_CLIPS, Order, Quad, Shape};
pub use renderer::{QuadMode, Render2dError, Renderer2d, SceneDepth};
pub use text::{GlyphTexture, TextColor, draw_glyphs, draw_text};
pub use view::{Camera2d, View2d};
