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
//! - [`Renderer2d`] writes the sorted quads into the upload ring (64 bytes each) and draws
//!   each batch with one indexed draw per 16384 quads; vertices are generated from the
//!   vertex index (no vertex buffers).
//! - [`Camera2d`] and [`View2d`]: a y-up world camera and y-down pixel space.
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
//! // renderer.add_pass(&mut fg, &mut ring, &bindless, &mut canvas, &camera.view(w, h), target, clear)?;
//! ```

mod canvas;
mod renderer;
mod sort;
mod view;

pub use canvas::{Blend2d, Canvas, CanvasStats, ClipRect, MAX_CLIPS, Order, Quad, Shape};
pub use renderer::{QuadMode, Renderer2d, RingFull};
pub use view::{Camera2d, View2d};
