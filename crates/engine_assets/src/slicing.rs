//! Cutting sprite sheets into slices (ADR 0125's E13-2; `docs/research/ux/2d.md`): by a
//! grid of cells, by islands of visible pixels, a rectangle fitted to the pixels around a
//! point, and cutting again so that slices which stay keep their names, pivots and borders
//! (Unity's Smart slicing), so scenes naming them (`hero.png#run_3`) do not break.
//!
//! Rectangles are `[left, top, width, height]` in pixels from the image's top left, as a
//! slice's `rect` is. Images are RGBA8, rows from the top; a pixel is visible when its
//! alpha is above 0.

use engine_scene::SliceImport;

/// A grid of cells: their size, the margin before the first, and the gap between two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceGrid {
    /// A cell's width and height in pixels.
    pub cell: [u32; 2],
    /// Pixels left of the first column and above the first row.
    pub margin: [u32; 2],
    /// Pixels between two columns and between two rows.
    pub gap: [u32; 2],
}

impl SliceGrid {
    /// Cells of `width` × `height` pixels, no margin, no gap.
    pub const fn cells(width: u32, height: u32) -> Self {
        Self {
            cell: [width, height],
            margin: [0, 0],
            gap: [0, 0],
        }
    }

    /// The grid that cuts an image of `size` pixels into `columns` × `rows` cells (the
    /// pixels a division leaves over fall off the right and bottom).
    pub fn count(size: [u32; 2], columns: u32, rows: u32) -> Self {
        Self::cells(
            (size[0] / columns.max(1)).max(1),
            (size[1] / rows.max(1)).max(1),
        )
    }
}

/// The cells of `grid` that fit inside an image of `size` pixels, row by row from the top
/// left; with the image's pixels `rgba`, those without a visible pixel left out.
pub fn grid_rects(size: [u32; 2], grid: SliceGrid, rgba: Option<&[u8]>) -> Vec<[u32; 4]> {
    let [cw, ch] = grid.cell;
    let mut rects = Vec::new();
    if cw == 0 || ch == 0 {
        return rects;
    }
    let mut y = grid.margin[1];
    while y + ch <= size[1] {
        let mut x = grid.margin[0];
        while x + cw <= size[0] {
            let rect = [x, y, cw, ch];
            if rgba.is_none_or(|p| any_visible(size, p, rect)) {
                rects.push(rect);
            }
            x += cw + grid.gap[0];
        }
        y += ch + grid.gap[1];
    }
    rects
}

/// Whether `rect` of the image holds a visible pixel.
fn any_visible(size: [u32; 2], rgba: &[u8], [x, y, w, h]: [u32; 4]) -> bool {
    (y..y + h).any(|py| (x..x + w).any(|px| alpha(size, rgba, px, py) > 0))
}

fn alpha(size: [u32; 2], rgba: &[u8], x: u32, y: u32) -> u8 {
    let i = (y as usize * size[0] as usize + x as usize) * 4 + 3;
    rgba.get(i).copied().unwrap_or(0)
}

/// The islands of visible pixels (touching across sides and corners), each its bounding
/// rectangle, those smaller than `min_size` pixels on both sides left out; in reading
/// order: rows (islands whose heights overlap) from the top, each from the left.
pub fn island_rects(size: [u32; 2], rgba: &[u8], min_size: u32) -> Vec<[u32; 4]> {
    let (w, h) = (size[0] as usize, size[1] as usize);
    let mut seen = vec![false; w * h];
    let mut stack = Vec::new();
    let mut rects = Vec::new();
    for start in 0..w * h {
        if seen[start] || rgba.get(start * 4 + 3).copied().unwrap_or(0) == 0 {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        let (mut lo, mut hi) = ([usize::MAX; 2], [0usize; 2]);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            lo = [lo[0].min(x), lo[1].min(y)];
            hi = [hi[0].max(x), hi[1].max(y)];
            for (dx, dy) in NEIGHBOURS {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                    continue;
                }
                let n = ny as usize * w + nx as usize;
                if !seen[n] && rgba.get(n * 4 + 3).copied().unwrap_or(0) > 0 {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        let rect = [
            lo[0] as u32,
            lo[1] as u32,
            (hi[0] - lo[0] + 1) as u32,
            (hi[1] - lo[1] + 1) as u32,
        ];
        if rect[2] >= min_size || rect[3] >= min_size {
            rects.push(rect);
        }
    }
    reading_order(&mut rects);
    rects
}

const NEIGHBOURS: [(isize, isize); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// `rects` in reading order: rows (rectangles whose heights overlap the row's) from the
/// top, each from the left.
fn reading_order(rects: &mut [[u32; 4]]) {
    rects.sort_by_key(|r| (r[1], r[0]));
    let mut start = 0;
    while start < rects.len() {
        let mut bottom = rects[start][1] + rects[start][3];
        let mut end = start + 1;
        while end < rects.len() && rects[end][1] < bottom {
            bottom = bottom.max(rects[end][1] + rects[end][3]);
            end += 1;
        }
        rects[start..end].sort_by_key(|r| r[0]);
        start = end;
    }
}

/// The rectangle of the island of visible pixels at pixel `at` (a double-click fitting a
/// slice to the drawing under it); none on a clear pixel.
pub fn fit_rect(size: [u32; 2], rgba: &[u8], at: [u32; 2]) -> Option<[u32; 4]> {
    if at[0] >= size[0] || at[1] >= size[1] || alpha(size, rgba, at[0], at[1]) == 0 {
        return None;
    }
    Some(flood(size, rgba, at))
}

/// The bounding rectangle of the island of visible pixels holding `at`.
fn flood(size: [u32; 2], rgba: &[u8], at: [u32; 2]) -> [u32; 4] {
    let (w, h) = (size[0] as usize, size[1] as usize);
    let mut seen = vec![false; w * h];
    let start = at[1] as usize * w + at[0] as usize;
    seen[start] = true;
    let mut stack = vec![start];
    let (mut lo, mut hi) = ([usize::MAX; 2], [0usize; 2]);
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        lo = [lo[0].min(x), lo[1].min(y)];
        hi = [hi[0].max(x), hi[1].max(y)];
        for (dx, dy) in NEIGHBOURS {
            let (nx, ny) = (x as isize + dx, y as isize + dy);
            if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                continue;
            }
            let n = ny as usize * w + nx as usize;
            if !seen[n] && rgba.get(n * 4 + 3).copied().unwrap_or(0) > 0 {
                seen[n] = true;
                stack.push(n);
            }
        }
    }
    [
        lo[0] as u32,
        lo[1] as u32,
        (hi[0] - lo[0] + 1) as u32,
        (hi[1] - lo[1] + 1) as u32,
    ]
}

/// The slices of a cut into `rects`, in their order, given the slices `old` the image had:
/// a rectangle that stays (the same, or overlapping an old one by more than half of both)
/// keeps that slice's name, pivot and border; a new one is named `{prefix}_{n}` with the
/// lowest numbers no slice has, standing on `pivot`.
pub fn reslice(
    old: &[SliceImport],
    rects: &[[u32; 4]],
    prefix: &str,
    pivot: [f32; 2],
) -> Vec<SliceImport> {
    let mut taken = vec![false; old.len()];
    let mut kept: Vec<Option<usize>> = Vec::with_capacity(rects.len());
    // Same rectangles first, then the most overlapping.
    for rect in rects {
        let same = old
            .iter()
            .enumerate()
            .position(|(k, s)| !taken[k] && s.rect == *rect);
        if let Some(k) = same {
            taken[k] = true;
        }
        kept.push(same);
    }
    for (i, rect) in rects.iter().enumerate() {
        if kept[i].is_some() {
            continue;
        }
        let best = old
            .iter()
            .enumerate()
            .filter(|(k, _)| !taken[*k])
            .map(|(k, s)| (k, overlap(s.rect, *rect)))
            .filter(|&(_, share)| share > 0.5)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((k, _)) = best {
            taken[k] = true;
            kept[i] = Some(k);
        }
    }
    let used: Vec<&str> = kept
        .iter()
        .flatten()
        .map(|&k| old[k].name.as_str())
        .collect();
    let mut names = Vec::new();
    let mut n = 0u32;
    let mut slices = Vec::with_capacity(rects.len());
    for (i, rect) in rects.iter().enumerate() {
        let slice = match kept[i] {
            Some(k) => SliceImport {
                rect: *rect,
                ..old[k].clone()
            },
            None => {
                let name = loop {
                    let name = format!("{prefix}_{n}");
                    n += 1;
                    if !used.contains(&name.as_str()) && !names.contains(&name) {
                        break name;
                    }
                };
                names.push(name.clone());
                SliceImport {
                    name,
                    rect: *rect,
                    pivot,
                    border: [0; 4],
                }
            }
        };
        slices.push(slice);
    }
    slices
}

/// How much `a` and `b` overlap: their overlap over the larger of their areas (above
/// one half, each covers more than half of the other).
fn overlap(a: [u32; 4], b: [u32; 4]) -> f32 {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let inter = f64::from(x1 - x0) * f64::from(y1 - y0);
    let larger = (f64::from(a[2]) * f64::from(a[3])).max(f64::from(b[2]) * f64::from(b[3]));
    (inter / larger.max(1.0)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image of `size` whose visible pixels `on(x, y)` says.
    fn image(size: [u32; 2], on: impl Fn(u32, u32) -> bool) -> Vec<u8> {
        (0..size[1])
            .flat_map(|y| (0..size[0]).map(move |x| (x, y)))
            .flat_map(|(x, y)| [9, 9, 9, if on(x, y) { 255 } else { 0 }])
            .collect()
    }

    /// Cells by size with margin and gap, by count, and empty cells left out.
    #[test]
    fn a_grid_cuts_cells_and_skips_empty_ones() {
        let grid = SliceGrid {
            cell: [16, 16],
            margin: [1, 2],
            gap: [2, 0],
        };
        let rects = grid_rects([52, 34], grid, None);
        assert_eq!(
            rects,
            [
                [1, 2, 16, 16],
                [19, 2, 16, 16],
                [1, 18, 16, 16],
                [19, 18, 16, 16]
            ]
        );
        assert_eq!(
            grid_rects([64, 32], SliceGrid::count([64, 32], 4, 1), None).len(),
            4
        );
        // Only the second of four 8-pixel cells has a visible pixel.
        let pixels = image([32, 8], |x, y| x == 10 && y == 3);
        assert_eq!(
            grid_rects([32, 8], SliceGrid::cells(8, 8), Some(&pixels)),
            [[8, 0, 8, 8]]
        );
    }

    /// Islands touching at corners are one; tiny ones left out; reading order by rows.
    #[test]
    fn islands_are_found_in_reading_order() {
        // Row one: a 3 × 3 block at (10, 1) and a diagonal pair at (1, 2)-(2, 3); row two:
        // a 4 × 2 bar at (0, 8); a lone pixel at (15, 9) too small for min 2.
        let pixels = image([16, 12], |x, y| {
            ((10..13).contains(&x) && (1..4).contains(&y))
                || (x, y) == (1, 2)
                || (x, y) == (2, 3)
                || ((0..4).contains(&x) && (8..10).contains(&y))
                || (x, y) == (15, 9)
        });
        let rects = island_rects([16, 12], &pixels, 2);
        assert_eq!(rects, [[1, 2, 2, 2], [10, 1, 3, 3], [0, 8, 4, 2]]);
        assert_eq!(island_rects([16, 12], &pixels, 0).len(), 4);
        // A double-click fits the island under it; a clear pixel fits nothing.
        assert_eq!(fit_rect([16, 12], &pixels, [11, 2]), Some([10, 1, 3, 3]));
        assert_eq!(fit_rect([16, 12], &pixels, [2, 3]), Some([1, 2, 2, 2]));
        assert_eq!(fit_rect([16, 12], &pixels, [6, 6]), None);
    }

    /// Cutting again keeps the slices that stay (same or mostly overlapping), new ones
    /// named after the free numbers.
    #[test]
    fn slicing_again_keeps_names() {
        let named = |name: &str, rect: [u32; 4]| SliceImport {
            name: String::from(name),
            rect,
            pivot: [0.5, 0.0],
            border: [1, 1, 1, 1],
        };
        let old = [
            named("idle", [0, 0, 16, 16]),
            named("run_0", [16, 0, 16, 16]),
            named("gone", [48, 0, 16, 16]),
        ];
        let rects = [
            [0, 0, 16, 16],
            [17, 0, 16, 16],
            [32, 0, 16, 16],
            [0, 16, 16, 16],
        ];
        let new = reslice(&old, &rects, "run", [0.5, 0.5]);
        let names: Vec<&str> = new.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["idle", "run_0", "run_1", "run_2"]);
        // Kept ones keep pivots and borders, with their new rectangles.
        assert_eq!(new[1].rect, [17, 0, 16, 16]);
        assert_eq!((new[1].pivot, new[1].border), ([0.5, 0.0], [1; 4]));
        assert_eq!((new[2].pivot, new[2].border), ([0.5, 0.5], [0; 4]));
    }
}
