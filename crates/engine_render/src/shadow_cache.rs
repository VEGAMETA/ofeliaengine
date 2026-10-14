//! The sun's cached cascades (ADR 0093): which texels of each cascade a frame draws.
//!
//! Each cascade keeps its depth across frames in a layer of its own, addressed
//! wrap-around: the texel of the light view's grid at column `c` and row `r` lives at
//! `((c − b.x) mod N, (r − b.y) mod N)` of the `N × N` layer, whatever the window around
//! the camera (`b`, the base, is where the window started when the layer was last drawn
//! whole, so a whole layer is always drawn in one piece). The window moves in whole texels
//! (ADR 0044's snapping), so after a move only the columns and rows it has not covered
//! before need drawing. Each such strip is a rectangle of the window; where it crosses the
//! layer's edge it is drawn as up to four pieces, each through the cascade's own projection
//! with the viewport shifted by `N` across the seam and a scissor around its part of the
//! layer.

use crate::shadow::Cascade;
use engine_core::math::Mat4;
use engine_rhi::ash::vk;

/// What a cascade's layer holds: the projection it was drawn with (anything else and its
/// texels mean nothing now) and where its window was.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Held {
    /// The light's view.
    pub light_from_world: Mat4,
    /// Half the window's side, in world units (what the fit keeps, [`crate::shadow::Keep`]).
    pub radius: f32,
    /// World size of a texel; with the depth range, the projection's scale.
    pub texel: f32,
    pub near: f32,
    pub far: f32,
    /// Texels on a side of the layer.
    pub resolution: u32,
    /// The window's first column and row in the light view's texel grid (rows grow towards
    /// the light view's −y, as the layer's do).
    pub origin: [i32; 2],
    /// The grid texel the layer's first texel holds (see the module documentation).
    pub base: [i32; 2],
}

impl Held {
    /// What `cascade` of the light view `light_from_world` holds once drawn into a layer of
    /// `resolution` texels (whole: its base is its window's origin).
    pub(crate) fn of(cascade: &Cascade, light_from_world: Mat4, resolution: u32) -> Self {
        let n = resolution as i32;
        // The center sits on whole texels: the quotient is an integer up to float noise.
        let column = (cascade.center.x / cascade.texel).round() as i32;
        let row = (cascade.center.y / cascade.texel).round() as i32;
        // The window's left edge is center − N/2 texels; its top row sees the light view's
        // +y edge, row −(center + N/2) (the projection's y points down, as Vulkan's).
        let origin = [column - n / 2, -row - n / 2];
        Self {
            light_from_world,
            radius: cascade.radius,
            texel: cascade.texel,
            near: cascade.near,
            far: cascade.far,
            resolution,
            origin,
            base: origin,
        }
    }

    /// Whether texels drawn for `self` hold the same depths for `other`: the same light,
    /// texel size, depth range and image.
    fn same_projection(&self, other: &Self) -> bool {
        self.light_from_world == other.light_from_world
            && self.texel.to_bits() == other.texel.to_bits()
            && self.near.to_bits() == other.near.to_bits()
            && self.far.to_bits() == other.far.to_bits()
            && self.resolution == other.resolution
    }

    /// Where the window's first column and row land in the layer (the uv offset of its
    /// lookups is this over `N`, plus a half).
    pub(crate) fn storage_origin(&self) -> [u32; 2] {
        let n = self.resolution as i32;
        [
            (self.origin[0] - self.base[0]).rem_euclid(n) as u32,
            (self.origin[1] - self.base[1]).rem_euclid(n) as u32,
        ]
    }

    /// The light view rectangle (`x0, y0, x1, y1`) a rectangle of window texels covers.
    pub(crate) fn light_rect(&self, window: [i32; 4]) -> [f32; 4] {
        let [x0, y0, x1, y1] = window;
        let t = self.texel;
        // Window row w is grid row origin + w, from −(row + 1) to −row texels in y.
        [
            (self.origin[0] + x0) as f32 * t,
            -((self.origin[1] + y1) as f32) * t,
            (self.origin[0] + x1) as f32 * t,
            -((self.origin[1] + y0) as f32) * t,
        ]
    }
}

/// A part of a cascade's layer to draw: the viewport (the cascade's projection, shifted
/// across the layer's seam) and the scissor around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Piece {
    /// Where the window's first texel lands: `(x, y)` of a viewport `N × N` in size.
    pub viewport: [i32; 2],
    /// The layer's texels drawn.
    pub scissor: vk::Rect2D,
}

/// A rectangle of the window to draw, in window texels (`x0..x1`, `y0..y1` within
/// `0..N`), and its pieces in the layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dirty {
    pub window: [i32; 4],
    pub pieces: [Piece; 4],
    pub piece_count: usize,
}

impl Dirty {
    pub(crate) fn pieces(&self) -> &[Piece] {
        &self.pieces[..self.piece_count]
    }
}

/// What a cascade draws this frame: up to two rectangles (a strip of columns, a strip of
/// rows), or the whole window (in one piece).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    pub rects: [Dirty; 2],
    pub rect_count: usize,
    /// The whole window is drawn (nothing usable was held).
    pub full: bool,
}

/// A plan that draws nothing.
pub(crate) const NOTHING: Plan = Plan {
    rects: [EMPTY_DIRTY; 2],
    rect_count: 0,
    full: false,
};

impl Plan {
    pub(crate) fn rects(&self) -> &[Dirty] {
        &self.rects[..self.rect_count]
    }
}

const EMPTY_PIECE: Piece = Piece {
    viewport: [0, 0],
    scissor: vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D {
            width: 0,
            height: 0,
        },
    },
};

const EMPTY_DIRTY: Dirty = Dirty {
    window: [0; 4],
    pieces: [EMPTY_PIECE; 4],
    piece_count: 0,
};

/// The window texel range `w0..w1` of one axis split at the layer's seam: up to two runs,
/// each `(first layer texel, length, viewport offset)` for a window whose first texel
/// lands at `s0` of `n`.
fn split_axis(w0: i32, w1: i32, s0: i32, n: i32) -> ([(i32, i32, i32); 2], usize) {
    let mut runs = [(0, 0, 0); 2];
    let mut count = 0;
    // Window texels before the seam land at s0 + w; those after it at s0 + w − n.
    let seam = n - s0;
    let (a0, a1) = (w0, w1.min(seam));
    if a1 > a0 {
        runs[count] = (s0 + a0, a1 - a0, s0);
        count += 1;
    }
    let (b0, b1) = (w0.max(seam), w1);
    if b1 > b0 {
        runs[count] = (s0 + b0 - n, b1 - b0, s0 - n);
        count += 1;
    }
    (runs, count)
}

/// The window rectangle `x0..x1 × y0..y1` with its pieces for a window whose first column
/// and row land at `storage` in a layer of `n` texels.
fn dirty(window: [i32; 4], storage: [u32; 2], n: i32) -> Dirty {
    let [x0, y0, x1, y1] = window;
    let (xs, xn) = split_axis(x0, x1, storage[0] as i32, n);
    let (ys, yn) = split_axis(y0, y1, storage[1] as i32, n);
    let mut out = Dirty {
        window,
        ..EMPTY_DIRTY
    };
    for &(sx, wx, vx) in &xs[..xn] {
        for &(sy, wy, vy) in &ys[..yn] {
            out.pieces[out.piece_count] = Piece {
                viewport: [vx, vy],
                scissor: vk::Rect2D {
                    offset: vk::Offset2D { x: sx, y: sy },
                    extent: vk::Extent2D {
                        width: wx as u32,
                        height: wy as u32,
                    },
                },
            };
            out.piece_count += 1;
        }
    }
    out
}

/// What a cascade that now holds `now` draws, given what its layer held (`held`, or
/// nothing): only the columns and rows its window has not covered before, else all of it
/// (then from the layer's first texel: `now`'s base becomes its origin; otherwise it keeps
/// `held`'s).
pub(crate) fn plan(held: Option<&Held>, now: &mut Held) -> Plan {
    let n = now.resolution as i32;
    let mut out = NOTHING;
    let moved = held
        .filter(|h| h.same_projection(now))
        .map(|h| {
            (
                h.base,
                [now.origin[0] - h.origin[0], now.origin[1] - h.origin[1]],
            )
        })
        .filter(|(_, d)| d[0].abs() < n && d[1].abs() < n);
    let Some((base, [dx, dy])) = moved else {
        now.base = now.origin;
        out.rects[0] = dirty([0, 0, n, n], [0, 0], n);
        out.rect_count = 1;
        out.full = true;
        return out;
    };
    now.base = base;
    let storage = now.storage_origin();
    // The window's texel w was the old window's w + d: new are those beyond the old one.
    let new_run = |d: i32| if d > 0 { (n - d, n) } else { (0, -d) };
    let (cx0, cx1) = new_run(dx);
    let (cy0, cy1) = new_run(dy);
    if cx1 > cx0 {
        out.rects[out.rect_count] = dirty([cx0, 0, cx1, n], storage, n);
        out.rect_count += 1;
    }
    if cy1 > cy0 {
        // The rows' strip without the columns already drawn.
        let (x0, x1) = if dx > 0 {
            (0, n - dx)
        } else if dx < 0 {
            (-dx, n)
        } else {
            (0, n)
        };
        if x1 > x0 {
            out.rects[out.rect_count] = dirty([x0, cy0, x1, cy1], storage, n);
            out.rect_count += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::Vec2;

    const N: u32 = 64;

    fn held(origin: [i32; 2]) -> Held {
        Held {
            light_from_world: Mat4::IDENTITY,
            radius: N as f32 * 0.25,
            texel: 0.5,
            near: 1.0,
            far: 20.0,
            resolution: N,
            origin,
            base: origin,
        }
    }

    /// A layer as the grid texel each of its texels holds (or none).
    type Image = Vec<Option<(i32, i32)>>;

    /// Draws `plan` for the window at `now` into `image`: each piece's scissored texels get
    /// the grid texel its viewport maps them to.
    fn draw(image: &mut Image, plan: &Plan, now: &Held) {
        let n = N as i32;
        for rect in plan.rects() {
            for piece in rect.pieces() {
                let s = piece.scissor;
                for y in s.offset.y..s.offset.y + s.extent.height as i32 {
                    for x in s.offset.x..s.offset.x + s.extent.width as i32 {
                        assert!((0..n).contains(&x) && (0..n).contains(&y), "{piece:?}");
                        // The viewport maps window texel w to image texel viewport + w.
                        let (wx, wy) = (x - piece.viewport[0], y - piece.viewport[1]);
                        assert!((0..n).contains(&wx) && (0..n).contains(&wy), "{piece:?}");
                        // Within the rectangle the piece belongs to.
                        let [x0, y0, x1, y1] = rect.window;
                        assert!((x0..x1).contains(&wx) && (y0..y1).contains(&wy));
                        image[(y * n + x) as usize] =
                            Some((now.origin[0] + wx, now.origin[1] + wy));
                    }
                }
            }
        }
    }

    /// Every texel of the window at `now` holds its own grid texel at
    /// `((c − base) mod N, (r − base) mod N)`.
    fn check(image: &Image, now: &Held) {
        let n = N as i32;
        for wy in 0..n {
            for wx in 0..n {
                let (c, r) = (now.origin[0] + wx, now.origin[1] + wy);
                let at = ((r - now.base[1]).rem_euclid(n) * n + (c - now.base[0]).rem_euclid(n))
                    as usize;
                assert_eq!(image[at], Some((c, r)), "window texel {wx}, {wy}");
            }
        }
    }

    #[test]
    fn a_whole_window_is_drawn_in_one_piece() {
        for origin in [[0, 0], [5, -3], [-70, 130], [63, 64]] {
            let mut now = held(origin);
            // Whatever the base was: a whole window starts the layer anew.
            now.base = [17, -40];
            let plan = plan(None, &mut now);
            assert!(plan.full);
            assert_eq!(now.base, now.origin);
            assert_eq!(now.storage_origin(), [0, 0]);
            let mut image = vec![None; (N * N) as usize];
            draw(&mut image, &plan, &now);
            check(&image, &now);
            let pieces = plan.rects()[0].pieces();
            assert_eq!(pieces.len(), 1);
            assert_eq!(pieces[0].viewport, [0, 0]);
            assert_eq!(
                pieces[0].scissor.extent.width * pieces[0].scissor.extent.height,
                N * N
            );
        }
    }

    #[test]
    fn moving_draws_only_the_new_texels_and_keeps_the_window_whole() {
        let n = N as i32;
        let mut now = held([3, -7]);
        let mut image = vec![None; (N * N) as usize];
        let first = plan(None, &mut now);
        draw(&mut image, &first, &now);
        // A walk with every direction, steps up to half the window, and the seam crossed.
        let mut seed = 12345u32;
        for _ in 0..400 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let dx = (seed >> 16) as i32 % (n / 2 + 1) - n / 4;
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let dy = (seed >> 16) as i32 % (n / 2 + 1) - n / 4;
            let mut next = held([now.origin[0] + dx, now.origin[1] + dy]);
            let p = plan(Some(&now), &mut next);
            assert_eq!(next.base, now.base);
            assert!(!p.full);
            let drawn: i32 = p
                .rects()
                .iter()
                .flat_map(Dirty::pieces)
                .map(|q| (q.scissor.extent.width * q.scissor.extent.height) as i32)
                .sum();
            // Exactly the texels the old window did not cover.
            let (ox, oy) = (n - dx.abs(), n - dy.abs());
            assert_eq!(drawn, n * n - ox * oy, "step {dx}, {dy}");
            draw(&mut image, &p, &next);
            check(&image, &next);
            now = next;
        }
    }

    #[test]
    fn standing_still_draws_nothing() {
        let now = held([10, 20]);
        let mut again = now;
        let p = plan(Some(&now), &mut again);
        assert!(!p.full);
        assert_eq!(p.rect_count, 0);
        assert_eq!(again, now);
    }

    #[test]
    fn another_projection_or_a_long_jump_draws_everything() {
        let now = held([0, 0]);
        let mut far = held([N as i32, 0]);
        assert!(plan(Some(&now), &mut far).full);
        let mut finer = Held { texel: 0.25, ..now };
        assert!(plan(Some(&now), &mut finer).full);
        let mut deeper = Held { far: 30.0, ..now };
        assert!(plan(Some(&now), &mut deeper).full);
        let mut turned = Held {
            light_from_world: Mat4::from_rotation_y(0.01),
            ..now
        };
        assert!(plan(Some(&now), &mut turned).full);
    }

    #[test]
    fn the_window_origin_follows_the_snapped_center() {
        let cascade = Cascade {
            center: Vec2::new(3.0 * 0.5, -2.0 * 0.5),
            radius: N as f32 * 0.5 / 2.0,
            near: 1.0,
            far: 20.0,
            limit: 0.9,
            texel: 0.5,
            split: 10.0,
        };
        let mut h = Held::of(&cascade, Mat4::IDENTITY, N);
        // Center column 3, row −(−2) = 2: the window starts half of it before.
        assert_eq!(h.origin, [3 - 32, 2 - 32]);
        assert_eq!(h.storage_origin(), [0, 0]);
        h.base = [-64, -64];
        assert_eq!(h.storage_origin(), [35, 34]);
        // The whole window's light view rectangle is the cascade's square.
        let r = h.light_rect([0, 0, N as i32, N as i32]);
        let c = cascade.center;
        assert_eq!(
            r,
            [
                c.x - cascade.radius,
                c.y - cascade.radius,
                c.x + cascade.radius,
                c.y + cascade.radius
            ]
        );
    }

    /// The window texels a light view point lands on through the cascade's own projection
    /// and a viewport at the window are those the grid says (rows down from the light
    /// view's +y): the planning's texels are the rasterizer's.
    #[test]
    fn window_texels_are_the_projections_pixels() {
        let cascade = Cascade {
            center: Vec2::new(-7.0 * 0.25, 11.0 * 0.25),
            radius: N as f32 * 0.25 / 2.0,
            near: 1.0,
            far: 40.0,
            limit: 0.9,
            texel: 0.25,
            split: 10.0,
        };
        let h = Held::of(&cascade, Mat4::IDENTITY, N);
        let clip_from_light = cascade.clip_from_light();
        let n = N as f32;
        for (fx, fy) in [
            (0.1, 0.2),
            (0.37, 0.91),
            (0.5, 0.5),
            (0.99, 0.01),
            (0.73, 0.64),
        ] {
            // A point inside the square, in light view space.
            let x = cascade.center.x - cascade.radius + fx * 2.0 * cascade.radius;
            let y = cascade.center.y - cascade.radius + fy * 2.0 * cascade.radius;
            let clip = clip_from_light * engine_core::math::Vec4::new(x, y, -10.0, 1.0);
            // A viewport of N × N at the origin: pixel = (ndc + 1) N / 2.
            let pixel = [
                ((clip.x / clip.w + 1.0) * n * 0.5).floor() as i32,
                ((clip.y / clip.w + 1.0) * n * 0.5).floor() as i32,
            ];
            let grid = [(x / h.texel).floor() as i32, (-y / h.texel).floor() as i32];
            assert_eq!(
                pixel,
                [grid[0] - h.origin[0], grid[1] - h.origin[1]],
                "{fx}, {fy}"
            );
            // And the texel's light view rectangle holds the point.
            let r = h.light_rect([pixel[0], pixel[1], pixel[0] + 1, pixel[1] + 1]);
            assert!(
                r[0] <= x && x <= r[2] && r[1] <= y && y <= r[3],
                "{r:?} {x} {y}"
            );
        }
    }
}
