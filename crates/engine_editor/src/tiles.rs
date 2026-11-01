//! Painting tile maps (ADR 0125's E13-3c): Tab over the viewport with a tile map
//! selected. The left button paints the brush, the right one erases, Ctrl+click picks the
//! cell under the pointer as the brush; each stroke is one undo step. Tab or Escape leaves.

use engine_core::math::Vec2;
use engine_ecs::Entity;

/// The status bar's line while painting.
pub(crate) const HELP: &str = "Tiles: left paints the brush  ·  right erases  ·  Ctrl+click \
                               picks a cell's tile  ·  Tiles: Pick Brush… in the palette  ·  \
                               Tab or Escape leaves";

/// Painting a tile map (Tab).
#[derive(Debug, Clone)]
pub(crate) struct TilePaint {
    /// The map painted.
    pub(crate) entity: Entity,
    /// A cell's size in metres (its tile set's tile over its pixels a metre).
    pub(crate) cell: Vec2,
    /// The stroke under way, if a button is held.
    pub(crate) stroke: Option<Stroke>,
    /// The cell under the pointer, if it is over the view.
    pub(crate) hovered: Option<(i32, i32)>,
}

/// A stroke: erasing or painting, and the last cell it reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stroke {
    pub(crate) erase: bool,
    pub(crate) last: (i32, i32),
}

/// The cells on the straight line from `a` to `b`, both ends included, in order: a fast
/// drag leaves no gaps (Bresenham's line).
pub(crate) fn line(a: (i32, i32), b: (i32, i32)) -> Vec<(i32, i32)> {
    let (mut x, mut y) = a;
    let (dx, dy) = ((b.0 - x).abs(), -(b.1 - y).abs());
    let (sx, sy) = (if x < b.0 { 1 } else { -1 }, if y < b.1 { 1 } else { -1 });
    let mut err = dx + dy;
    let mut out = Vec::with_capacity((dx - dy + 1) as usize);
    loop {
        out.push((x, y));
        if (x, y) == b {
            return out;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// The cell of a map holding map point `p` (from the map's origin, metres) for cells of
/// `cell` metres.
pub(crate) fn cell_at(p: Vec2, cell: Vec2) -> (i32, i32) {
    let c = (p / cell.max(Vec2::splat(1e-6))).floor();
    (c.x as i32, c.y as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines reach every cell between their ends, in order, both ways.
    #[test]
    fn lines_leave_no_gaps() {
        assert_eq!(line((0, 0), (3, 0)), [(0, 0), (1, 0), (2, 0), (3, 0)]);
        assert_eq!(line((2, 2), (2, 2)), [(2, 2)]);
        let diagonal = line((0, 0), (-3, 2));
        assert_eq!(diagonal.first(), Some(&(0, 0)));
        assert_eq!(diagonal.last(), Some(&(-3, 2)));
        for w in diagonal.windows(2) {
            assert!((w[0].0 - w[1].0).abs() <= 1 && (w[0].1 - w[1].1).abs() <= 1);
        }
    }

    /// Cells below and left of the origin are negative; half-metre cells.
    #[test]
    fn cells_are_found_on_both_sides_of_the_origin() {
        let one = Vec2::ONE;
        assert_eq!(cell_at(Vec2::new(0.5, 0.5), one), (0, 0));
        assert_eq!(cell_at(Vec2::new(-0.5, -0.01), one), (-1, -1));
        assert_eq!(cell_at(Vec2::new(1.2, 0.7), Vec2::splat(0.5)), (2, 1));
    }
}
