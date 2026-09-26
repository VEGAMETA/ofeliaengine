//! Glyph rasterization: outlines from skrifa, flattened to lines, accumulated as signed
//! area per pixel (the technique described in Raph Levien's "font-rs" write-up), then
//! integrated along each row into 8-bit coverage. Exact area coverage, no supersampling.

use crate::font::Font;
use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};

/// A rendered glyph: 8-bit coverage, row-major, `width × height`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlyphBitmap {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Offset from the pen position to the bitmap's left edge, in pixels.
    pub left: i32,
    /// Offset from the baseline up to the bitmap's top edge, in pixels.
    pub top: i32,
    /// Coverage, 0..=255.
    pub coverage: Vec<u8>,
}

/// Accumulates signed area of line segments; reusable between glyphs.
#[derive(Debug, Default)]
pub struct Rasterizer {
    width: usize,
    height: usize,
    acc: Vec<f32>,
}

impl Rasterizer {
    /// An empty rasterizer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears to a `width × height` canvas.
    pub fn reset(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.acc.clear();
        // One extra cell per row absorbs contributions at the right edge.
        self.acc.resize(width * height + 2, 0.0);
    }

    /// Adds a line from `p0` to `p1` in pixel coordinates (y down).
    pub fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if (p0.1 - p1.1).abs() <= f32::EPSILON {
            return;
        }
        let (dir, p0, p1) = if p0.1 < p1.1 {
            (1.0f32, p0, p1)
        } else {
            (-1.0f32, p1, p0)
        };
        let w = self.width as f32;
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let mut x = p0.0;
        let y_start = p0.1.max(0.0);
        if p0.1 < 0.0 {
            x -= p0.1 * dxdy;
        }
        let y_end = (p1.1.ceil() as usize).min(self.height);
        let mut y = y_start as usize;
        while y < y_end {
            let row = y * self.width;
            let dy = ((y + 1) as f32).min(p1.1) - (y as f32).max(p0.1);
            let x_next = x + dxdy * dy;
            let d = dy * dir;
            let (x0, x1) = if x < x_next { (x, x_next) } else { (x_next, x) };
            let (x0, x1) = (x0.clamp(0.0, w), x1.clamp(0.0, w));
            let x0_floor = x0.floor();
            let x0i = x0_floor as usize;
            let x1_ceil = x1.ceil();
            let x1i = x1_ceil as usize;
            if x1i <= x0i + 1 {
                // The segment stays within one pixel column on this row.
                let xm = 0.5 * (x0 + x1) - x0_floor;
                self.acc[row + x0i] += d - d * xm;
                self.acc[row + x0i + 1] += d * xm;
            } else {
                let s = 1.0 / (x1 - x0);
                let x0f = x0 - x0_floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = x1 - x1_ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                self.acc[row + x0i] += d * a0;
                if x1i == x0i + 2 {
                    self.acc[row + x0i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    self.acc[row + x0i + 1] += d * (a1 - a0);
                    for xi in x0i + 2..x1i - 1 {
                        self.acc[row + xi] += d * s;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    self.acc[row + x1i - 1] += d * (1.0 - a2 - am);
                }
                self.acc[row + x1i] += d * am;
            }
            x = x_next;
            y += 1;
        }
    }

    /// Integrates the accumulated area into coverage (non-zero winding, clamped).
    pub fn coverage(&self, out: &mut Vec<u8>) {
        out.clear();
        out.reserve(self.width * self.height);
        let mut sum = 0.0f32;
        for &a in &self.acc[..self.width * self.height] {
            sum += a;
            out.push((sum.abs().min(1.0) * 255.0 + 0.5) as u8);
        }
    }
}

/// Flattens curves into the rasterizer, translating font units (y up) into bitmap pixels.
struct Pen<'a> {
    raster: &'a mut Rasterizer,
    /// Bitmap x of the glyph origin.
    dx: f32,
    /// Bitmap y of the baseline.
    dy: f32,
    start: (f32, f32),
    current: (f32, f32),
}

impl Pen<'_> {
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        (x + self.dx, self.dy - y)
    }
}

/// Flattening tolerance in pixels.
const TOLERANCE: f32 = 0.2;

impl OutlinePen for Pen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.map(x, y);
        self.start = p;
        self.current = p;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.map(x, y);
        self.raster.line(self.current, p);
        self.current = p;
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let p0 = self.current;
        let p1 = self.map(cx, cy);
        let p2 = self.map(x, y);
        let dd = ((p0.0 - 2.0 * p1.0 + p2.0).powi(2) + (p0.1 - 2.0 * p1.1 + p2.1).powi(2)).sqrt();
        let n = ((dd / (8.0 * TOLERANCE)).sqrt().ceil() as usize).clamp(1, 64);
        let mut prev = p0;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            let p = (
                mt * mt * p0.0 + 2.0 * mt * t * p1.0 + t * t * p2.0,
                mt * mt * p0.1 + 2.0 * mt * t * p1.1 + t * t * p2.1,
            );
            self.raster.line(prev, p);
            prev = p;
        }
        self.current = p2;
    }

    fn curve_to(&mut self, c0x: f32, c0y: f32, c1x: f32, c1y: f32, x: f32, y: f32) {
        let p0 = self.current;
        let p1 = self.map(c0x, c0y);
        let p2 = self.map(c1x, c1y);
        let p3 = self.map(x, y);
        let dd = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
            ((a.0 - 2.0 * b.0 + c.0).powi(2) + (a.1 - 2.0 * b.1 + c.1).powi(2)).sqrt()
        };
        let d = dd(p0, p1, p2).max(dd(p1, p2, p3));
        let n = ((d * 0.75 / TOLERANCE).sqrt().ceil() as usize).clamp(1, 64);
        let mut prev = p0;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            let (a, b, c, e) = (mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t);
            let p = (
                a * p0.0 + b * p1.0 + c * p2.0 + e * p3.0,
                a * p0.1 + b * p1.1 + c * p2.1 + e * p3.1,
            );
            self.raster.line(prev, p);
            prev = p;
        }
        self.current = p3;
    }

    fn close(&mut self) {
        if self.current != self.start {
            self.raster.line(self.current, self.start);
        }
        self.current = self.start;
    }
}

/// Bounding box of an outline in pixels (font y up).
#[derive(Default)]
struct Bounds {
    min: (f32, f32),
    max: (f32, f32),
    any: bool,
}

impl Bounds {
    fn add(&mut self, x: f32, y: f32) {
        if self.any {
            self.min = (self.min.0.min(x), self.min.1.min(y));
            self.max = (self.max.0.max(x), self.max.1.max(y));
        } else {
            self.min = (x, y);
            self.max = (x, y);
            self.any = true;
        }
    }
}

impl OutlinePen for Bounds {
    fn move_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        // Control points bound the curve.
        self.add(cx, cy);
        self.add(x, y);
    }
    fn curve_to(&mut self, c0x: f32, c0y: f32, c1x: f32, c1y: f32, x: f32, y: f32) {
        self.add(c0x, c0y);
        self.add(c1x, c1y);
        self.add(x, y);
    }
    fn close(&mut self) {}
}

/// Renders `glyph` of `font` at `size` pixels, with the pen at a fractional x offset
/// `subpixel_x` (0..1). Returns `None` for glyphs without an outline (spaces) or bitmaps.
pub fn rasterize(
    font: &Font,
    glyph: u16,
    size: f32,
    subpixel_x: f32,
    raster: &mut Rasterizer,
    out: &mut GlyphBitmap,
) -> Option<()> {
    let font_ref = font.font_ref()?;
    let outlines = font_ref.outline_glyphs();
    let outline = outlines.get(skrifa::GlyphId::new(u32::from(glyph)))?;
    let settings = || DrawSettings::unhinted(Size::new(size), LocationRef::default());
    let mut bounds = Bounds::default();
    outline.draw(settings(), &mut bounds).ok()?;
    if !bounds.any || bounds.max.0 <= bounds.min.0 || bounds.max.1 <= bounds.min.1 {
        return None;
    }
    let left = (bounds.min.0 + subpixel_x).floor() as i32;
    let right = (bounds.max.0 + subpixel_x).ceil() as i32;
    let top = bounds.max.1.ceil() as i32;
    let bottom = bounds.min.1.floor() as i32;
    let width = (right - left).max(1) as usize;
    let height = (top - bottom).max(1) as usize;
    if width > 4096 || height > 4096 {
        return None;
    }
    raster.reset(width, height);
    let mut pen = Pen {
        raster,
        dx: subpixel_x - left as f32,
        dy: top as f32,
        start: (0.0, 0.0),
        current: (0.0, 0.0),
    };
    outline.draw(settings(), &mut pen).ok()?;
    pen.raster.coverage(&mut out.coverage);
    out.width = width as u32;
    out.height = height as u32;
    out.left = left;
    out.top = top;
    Some(())
}
