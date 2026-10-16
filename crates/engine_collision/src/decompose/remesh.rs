//! Any mesh made a closed manifold for the decomposition (ADR 0101), as CoACD does with
//! OpenVDB, without it:
//!
//! 1. the unsigned distance to the triangles at the points of a grid, in a band of three
//!    cells around them;
//! 2. the outside found by a flood fill from the grid's border through points farther than
//!    the offset (an open mesh becomes a closed shell around its surface);
//! 3. the surface `offset` out from it by marching tetrahedra: six tetrahedra a cell around
//!    its main diagonal (the same in every cell, so neighbours agree), a vertex on each edge
//!    that changes sign, shared by the cells around it. No case is ambiguous, so the result
//!    is closed.
//!
//! The grid has `cells` cells across the mesh's longest side, CoACD's 100 by default, and
//! the offset is CoACD's 0.55 of a cell. Written for this engine; no code was copied.

use super::bvh::closest_on_triangle;
use super::clip::Solid;
use super::map;
use engine_core::jobs::ThreadPool;
use engine_core::math::DVec3;
use std::collections::{HashMap, VecDeque};

/// Whether `solid` is closed and wound outward: every edge once each way, a positive volume.
pub(crate) fn closed_manifold(solid: &Solid) -> bool {
    let mut edges: HashMap<(u32, u32), u32> = HashMap::with_capacity(solid.triangles.len() * 3);
    for t in &solid.triangles {
        if t[0] == t[1] || t[1] == t[2] || t[0] == t[2] {
            return false;
        }
        for k in 0..3 {
            *edges.entry((t[k], t[(k + 1) % 3])).or_insert(0) += 1;
        }
    }
    !solid.triangles.is_empty()
        && edges
            .iter()
            .all(|(&(a, b), &n)| n == 1 && edges.get(&(b, a)) == Some(&1))
        && solid.volume() > 0.0
}

/// The six tetrahedra of a cell around its diagonal from corner 0 to corner 7 (corner bits:
/// x, y, z).
const TETRAHEDRA: [[usize; 4]; 6] = [
    [0, 1, 3, 7],
    [0, 3, 2, 7],
    [0, 2, 6, 7],
    [0, 6, 4, 7],
    [0, 4, 5, 7],
    [0, 5, 1, 7],
];

/// The closed surface `offset` (in cells) out from `points` and `triangles` (see the
/// module's documentation). The distances are found per slab of the grid on `jobs`.
pub(crate) fn remesh(
    points: &[DVec3],
    triangles: &[[u32; 3]],
    cells: usize,
    offset: f64,
    jobs: Option<&ThreadPool>,
) -> Solid {
    let (lo, hi) = points.iter().fold(
        (DVec3::splat(f64::MAX), DVec3::splat(f64::MIN)),
        |(lo, hi), &p| (lo.min(p), hi.max(p)),
    );
    let h = (hi - lo).max_element().max(1e-12) / cells as f64;
    let band = 3.0 * h;
    let pad = 4.0 * h + offset * h;
    // Off the mesh's own grid by fractions of a cell: axis-aligned faces of a box-like mesh
    // would otherwise put grid points exactly on the offset surface, and their crossings
    // (several edges at one point) make slivers that simplification cannot collapse.
    let origin = lo - DVec3::splat(pad) - DVec3::new(0.123_456, 0.271_828, 0.314_159) * h;
    let dims = ((hi - lo + DVec3::splat(2.0 * pad)) / h).ceil().as_uvec3() + 1;
    let (nx, ny, nz) = (dims.x as usize, dims.y as usize, dims.z as usize);
    let index = |i: usize, j: usize, k: usize| (k * ny + j) * nx + i;
    let point =
        |i: usize, j: usize, k: usize| origin + DVec3::new(i as f64, j as f64, k as f64) * h;
    let far = 4.0 * band;
    // 1. Distances in a band around every triangle, a slab of z layers at a time (each
    //    slab's points are contiguous, and only its own task writes them).
    let ranges: Vec<_> = triangles
        .iter()
        .map(|t| {
            let [a, b, c] = t.map(|i| points[i as usize]);
            let tlo = a.min(b).min(c) - DVec3::splat(band);
            let thi = a.max(b).max(c) + DVec3::splat(band);
            let first = ((tlo - origin) / h).floor().max(DVec3::ZERO).as_uvec3();
            let last = ((thi - origin) / h).ceil().as_uvec3().min(dims - 1);
            (first, last)
        })
        .collect();
    const SLAB: usize = 8;
    let slabs = map(nz.div_ceil(SLAB), jobs, |s| {
        let (k0, k1) = (s * SLAB, ((s + 1) * SLAB).min(nz));
        let mut slab = vec![far; nx * ny * (k1 - k0)];
        for (t, &(first, last)) in triangles.iter().zip(&ranges) {
            let (z0, z1) = ((first.z as usize).max(k0), (last.z as usize).min(k1 - 1));
            if z0 > z1 {
                continue;
            }
            let [a, b, c] = t.map(|i| points[i as usize]);
            for k in z0..=z1 {
                for j in first.y as usize..=last.y as usize {
                    for i in first.x as usize..=last.x as usize {
                        let p = point(i, j, k);
                        let d = closest_on_triangle(p, a, b, c).distance(p);
                        let slot = &mut slab[index(i, j, k - k0)];
                        if d < *slot {
                            *slot = d;
                        }
                    }
                }
            }
        }
        slab
    });
    let distance: Vec<f64> = slabs.concat();
    // 2. The outside: from the border through points farther than the offset.
    let threshold = offset * h;
    let mut outside = vec![false; nx * ny * nz];
    let mut queue = VecDeque::new();
    for k in 0..nz {
        for j in 0..ny {
            for i in 0..nx {
                let border =
                    i == 0 || j == 0 || k == 0 || i == nx - 1 || j == ny - 1 || k == nz - 1;
                let at = index(i, j, k);
                if border && distance[at] > threshold {
                    outside[at] = true;
                    queue.push_back((i, j, k));
                }
            }
        }
    }
    while let Some((i, j, k)) = queue.pop_front() {
        let mut visit = |i: usize, j: usize, k: usize| {
            let at = index(i, j, k);
            if !outside[at] && distance[at] > threshold {
                outside[at] = true;
                queue.push_back((i, j, k));
            }
        };
        if i > 0 {
            visit(i - 1, j, k);
        }
        if i + 1 < nx {
            visit(i + 1, j, k);
        }
        if j > 0 {
            visit(i, j - 1, k);
        }
        if j + 1 < ny {
            visit(i, j + 1, k);
        }
        if k > 0 {
            visit(i, j, k - 1);
        }
        if k + 1 < nz {
            visit(i, j, k + 1);
        }
    }
    // The field whose zero is the surface: positive outside, never exactly zero (a crossing
    // then lies inside its edge, never on a grid point several edges share).
    let tiny = 1e-9 * h;
    let field: Vec<f64> = distance
        .iter()
        .zip(&outside)
        .map(|(&d, &o)| {
            // Within the offset of the surface (inside or out), d - offset is the distance to
            // the offset surface; beyond it and not reached from outside, the point is enclosed.
            let f = if o || d < threshold {
                d - threshold
            } else {
                -d - threshold
            };
            if f.abs() < tiny { tiny.copysign(f) } else { f }
        })
        .collect();
    // 3. Marching tetrahedra.
    let mut out = Solid::default();
    let mut on_edge: HashMap<(usize, usize), u32> = HashMap::new();
    let mut vertex = |a: usize, b: usize, pa: DVec3, pb: DVec3, out: &mut Solid| -> u32 {
        let key = (a.min(b), a.max(b));
        *on_edge.entry(key).or_insert_with(|| {
            let (fa, fb) = (field[a], field[b]);
            let t = (fa / (fa - fb)).clamp(0.0, 1.0);
            out.points.push(pa + (pb - pa) * t);
            (out.points.len() - 1) as u32
        })
    };
    for k in 0..nz - 1 {
        for j in 0..ny - 1 {
            for i in 0..nx - 1 {
                let corner = |c: usize| (i + (c & 1), j + ((c >> 1) & 1), k + ((c >> 2) & 1));
                let ids: [usize; 8] = core::array::from_fn(|c| {
                    let (x, y, z) = corner(c);
                    index(x, y, z)
                });
                let inside = ids.map(|id| field[id] < 0.0);
                if inside.iter().all(|&x| x) || inside.iter().all(|&x| !x) {
                    continue;
                }
                let pos: [DVec3; 8] = core::array::from_fn(|c| {
                    let (x, y, z) = corner(c);
                    point(x, y, z)
                });
                for tet in TETRAHEDRA {
                    let (ins, outs): (Vec<usize>, Vec<usize>) =
                        tet.iter().partition(|&&c| inside[c]);
                    let emit = |verts: [u32; 3], toward: DVec3, out: &mut Solid| {
                        let [a, b, c] = verts.map(|v| out.points[v as usize]);
                        let n = (b - a).cross(c - a);
                        if n.dot(toward) >= 0.0 {
                            out.triangles.push(verts);
                        } else {
                            out.triangles.push([verts[0], verts[2], verts[1]]);
                        }
                    };
                    match ins.len() {
                        1 | 3 => {
                            // One corner alone: a triangle around it, facing out.
                            let (lone, others, sign) = if ins.len() == 1 {
                                (ins[0], &outs, 1.0)
                            } else {
                                (outs[0], &ins, -1.0)
                            };
                            let v = [0, 1, 2].map(|m| {
                                vertex(
                                    ids[lone],
                                    ids[others[m]],
                                    pos[lone],
                                    pos[others[m]],
                                    &mut out,
                                )
                            });
                            let centre = (out.points[v[0] as usize]
                                + out.points[v[1] as usize]
                                + out.points[v[2] as usize])
                                / 3.0;
                            emit(v, (centre - pos[lone]) * sign, &mut out);
                        }
                        2 => {
                            // A quad between the two inside and the two outside corners.
                            let (i0, i1, o0, o1) = (ins[0], ins[1], outs[0], outs[1]);
                            let v00 = vertex(ids[i0], ids[o0], pos[i0], pos[o0], &mut out);
                            let v01 = vertex(ids[i0], ids[o1], pos[i0], pos[o1], &mut out);
                            let v11 = vertex(ids[i1], ids[o1], pos[i1], pos[o1], &mut out);
                            let v10 = vertex(ids[i1], ids[o0], pos[i1], pos[o0], &mut out);
                            let toward = (pos[o0] + pos[o1] - pos[i0] - pos[i1]) * 0.5;
                            emit([v00, v01, v11], toward, &mut out);
                            emit([v00, v11, v10], toward, &mut out);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::clip::tests::cuboid;

    #[test]
    fn a_closed_box_stays_a_box_and_an_open_one_becomes_a_shell() {
        let solid = cuboid(DVec3::ZERO, DVec3::ONE);
        assert!(closed_manifold(&solid));
        let out = remesh(&solid.points, &solid.triangles, 32, 0.55, None);
        assert!(closed_manifold(&out), "remeshed box not closed");
        // The box 0.55 of a cell (1/32) larger on every side, its edges rounded: the volume of
        // the box grown by 0.0172, minus a little at the edges.
        let grown = (1.0 + 2.0 * 0.55 / 32.0f64).powi(3);
        assert!(
            (out.volume() - grown).abs() < 0.03,
            "{} against {grown}",
            out.volume()
        );
        // Without its top: a closed shell around the five faces, thin.
        let open = Solid {
            points: solid.points.clone(),
            triangles: solid.triangles[..10].to_vec(),
        };
        assert!(!closed_manifold(&open));
        let shell = remesh(&open.points, &open.triangles, 32, 0.55, None);
        assert!(closed_manifold(&shell));
        assert!(
            shell.volume() < 0.2 && shell.volume() > 0.05,
            "{}",
            shell.volume()
        );
    }
}
