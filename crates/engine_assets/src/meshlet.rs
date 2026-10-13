//! Meshlets (ADR 0085): clusters of up to [`MAX_VERTICES`] vertices and [`MAX_TRIANGLES`]
//! triangles per detail level, made at cook time and culled per view on the GPU (frustum,
//! normal cone, occlusion; `docs/research/meshlets.md`).
//!
//! A level's index buffer is reordered meshlet by meshlet, with global indices: a meshlet is
//! a contiguous range of it, drawn by one indexed draw on GPUs without mesh shaders. The
//! vertices keep their order, so a coarser level still uses a prefix of them (ADR 0076).
//!
//! Inside a meshlet the triangles are in the order meshoptimizer's vertex cache optimizer
//! gives them, not its meshlet optimizer's (made for mesh shaders' rasterization): draws of
//! a whole level read the same buffer. On the test scenes (the `meshlet_stats` example) a
//! level then transforms 1-9 % more vertices per triangle than in the vertex cache
//! optimizer's order alone, against 4-18 % with the meshlet optimizer's order
//! (meshoptimizer's cache models, 2026-10-14).

use bytemuck::{Pod, Zeroable};

/// Most vertices of a meshlet (meshoptimizer's advice for mesh shaders; the indexed draws
/// do not mind).
pub const MAX_VERTICES: usize = 64;
/// Most triangles of a meshlet (a multiple of 4, as meshoptimizer 0.25 needs).
pub const MAX_TRIANGLES: usize = 124;
/// How much meshoptimizer favours tight normal cones over compact spheres.
const CONE_WEIGHT: f32 = 0.25;

/// A meshlet as the GPU reads it (32 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Meshlet {
    /// Its bounding sphere in object space: center.
    pub center: [f32; 3],
    /// And radius.
    pub radius: f32,
    /// Its normal cone: axis (x, y, z) and cutoff as signed 8-bit values over 127,
    /// rounded so the test stays conservative (meshoptimizer's). Seen from `eye`, the
    /// meshlet faces away when `dot(center − eye, axis) ≥ cutoff · |center − eye| +
    /// radius`; a cutoff of 127 means no cone (every direction).
    pub cone: [i8; 4],
    /// Its first index in the level's index buffer.
    pub first_index: u32,
    /// Its triangles.
    pub triangles: u32,
    /// 0.
    pub reserved: u32,
}

const _: () = assert!(size_of::<Meshlet>() == 32);

/// Reorders `indices` (triangles over `positions`) meshlet by meshlet and returns the
/// meshlets, in that order. Nothing changes (and no meshlets) for an empty mesh or one
/// meshoptimizer cannot cluster whole.
pub fn build_meshlets(positions: &[[f32; 3]], indices: &mut Vec<u32>) -> Vec<Meshlet> {
    if indices.len() < 3
        || !indices.len().is_multiple_of(3)
        || indices.iter().any(|&i| i as usize >= positions.len())
    {
        return Vec::new();
    }
    let Ok(adapter) = meshopt::VertexDataAdapter::new(bytemuck::cast_slice(positions), 12, 0)
    else {
        return Vec::new();
    };
    let built =
        meshopt::build_meshlets(indices, &adapter, MAX_VERTICES, MAX_TRIANGLES, CONE_WEIGHT);
    let mut reordered = Vec::with_capacity(indices.len());
    let mut meshlets = Vec::with_capacity(built.len());
    for m in built.iter() {
        let bounds = meshopt::compute_meshlet_bounds(m, &adapter);
        let first_index = reordered.len() as u32;
        let local: Vec<u32> = m.triangles.iter().map(|&l| u32::from(l)).collect();
        let local = meshopt::optimize_vertex_cache(&local, m.vertices.len());
        reordered.extend(local.iter().map(|&l| m.vertices[l as usize]));
        meshlets.push(Meshlet {
            center: bounds.center,
            radius: bounds.radius,
            cone: [
                bounds.cone_axis_s8[0],
                bounds.cone_axis_s8[1],
                bounds.cone_axis_s8[2],
                bounds.cone_cutoff_s8,
            ],
            first_index,
            triangles: (m.triangles.len() / 3) as u32,
            reserved: 0,
        });
    }
    // Every triangle once (meshoptimizer keeps them all): otherwise the order stays.
    if reordered.len() != indices.len() {
        return Vec::new();
    }
    *indices = reordered;
    meshlets
}

/// Whether `meshlets` cover a level of `index_count` indices exactly, in order: what a
/// pack's tables must satisfy.
pub fn meshlets_cover(meshlets: &[Meshlet], index_count: usize) -> bool {
    let mut next = 0usize;
    for m in meshlets {
        let count = m.triangles as usize * 3;
        if m.first_index as usize != next
            || m.triangles == 0
            || m.triangles as usize > MAX_TRIANGLES
        {
            return false;
        }
        next += count;
    }
    next == index_count
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `n × n` grid of quads in the z = 0 plane, facing +z.
    fn grid(n: usize) -> (Vec<[f32; 3]>, Vec<u32>) {
        let mut positions = Vec::new();
        for y in 0..=n {
            for x in 0..=n {
                positions.push([x as f32, y as f32, 0.0]);
            }
        }
        let mut indices = Vec::new();
        let row = (n + 1) as u32;
        for y in 0..n as u32 {
            for x in 0..n as u32 {
                let a = y * row + x;
                indices.extend([a, a + 1, a + row + 1, a, a + row + 1, a + row]);
            }
        }
        (positions, indices)
    }

    #[test]
    fn meshlets_cover_every_triangle_once_and_bound_it() {
        let (positions, original) = grid(32);
        let mut indices = original.clone();
        let meshlets = build_meshlets(&positions, &mut indices);
        assert!(
            meshlets.len() >= 2048 / MAX_TRIANGLES,
            "{} meshlets",
            meshlets.len()
        );
        assert!(meshlets_cover(&meshlets, indices.len()));
        // The same triangles with the same winding (up to rotation), only reordered.
        let key = |t: &[u32]| {
            let mut t = [t[0], t[1], t[2]];
            let first = (0..3).min_by_key(|&k| t[k]).unwrap_or(0);
            t.rotate_left(first);
            t
        };
        let mut a: Vec<_> = original.chunks(3).map(key).collect();
        let mut b: Vec<_> = indices.chunks(3).map(key).collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
        for m in &meshlets {
            let range = m.first_index as usize..(m.first_index + m.triangles * 3) as usize;
            let mut vertices: Vec<u32> = indices[range.clone()].to_vec();
            vertices.sort_unstable();
            vertices.dedup();
            assert!(vertices.len() <= MAX_VERTICES);
            for &i in &indices[range] {
                let p = positions[i as usize];
                let d = (0..3)
                    .map(|k| (p[k] - m.center[k]).powi(2))
                    .sum::<f32>()
                    .sqrt();
                assert!(d <= m.radius * 1.001 + 1e-4, "a vertex outside its sphere");
            }
            // A flat grid facing +z: the cone's axis is +z (along the normals) and its
            // cutoff about 0 (a half-angle of 0: rejected from anywhere behind the plane).
            assert!(m.cone[2] > 100 && m.cone[3] <= 2, "cone {:?}", m.cone);
        }
    }

    #[test]
    fn the_cone_rejects_from_behind_only() {
        let (positions, mut indices) = grid(8);
        let meshlets = build_meshlets(&positions, &mut indices);
        let faces_away = |m: &Meshlet, eye: [f32; 3]| {
            let axis = [0, 1, 2].map(|k| f32::from(m.cone[k]) / 127.0);
            let cutoff = f32::from(m.cone[3]) / 127.0;
            let d = [0, 1, 2].map(|k| m.center[k] - eye[k]);
            let length = d.iter().map(|x| x * x).sum::<f32>().sqrt();
            d.iter().zip(axis).map(|(a, b)| a * b).sum::<f32>() >= cutoff * length + m.radius
        };
        for m in &meshlets {
            assert!(!faces_away(m, [4.0, 4.0, 10.0]), "seen from the front");
            assert!(faces_away(m, [4.0, 4.0, -10.0]), "seen from behind");
        }
    }

    #[test]
    fn nothing_to_cluster_changes_nothing() {
        let mut empty = Vec::new();
        assert!(build_meshlets(&[[0.0; 3]; 3], &mut empty).is_empty());
        let mut bad = vec![0, 1, 7];
        assert!(build_meshlets(&[[0.0; 3]; 3], &mut bad).is_empty());
        assert_eq!(bad, [0, 1, 7]);
        assert!(!meshlets_cover(&[], 3));
    }
}
