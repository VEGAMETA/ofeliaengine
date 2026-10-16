//! Triangle mesh helpers for colliders (ADR 0101): the volume a mesh encloses, whether it
//! is closed, and its vertices welded within a distance (render meshes split vertices at
//! seams of normals and texture coordinates; colliders want them joined).

use engine_core::math::{DVec3, Vec3};
use std::collections::HashMap;

/// The volume `triangles` enclose (signed: negative when wound inside out). Meaningful when
/// the mesh is closed ([`is_closed`]).
pub fn volume(vertices: &[Vec3], triangles: &[[u32; 3]]) -> f32 {
    let Some(&origin) = vertices.first() else {
        return 0.0;
    };
    let o = origin.as_dvec3();
    let mut six = 0.0;
    for t in triangles {
        let [a, b, c] = t.map(|i| vertices[i as usize].as_dvec3() - o);
        six += a.dot(b.cross(c));
    }
    (six / 6.0) as f32
}

/// Whether every edge of `triangles` is shared by exactly two of them, in opposite
/// directions (a closed, consistently wound surface).
pub fn is_closed(triangles: &[[u32; 3]]) -> bool {
    let mut edges: HashMap<(u32, u32), i32> = HashMap::with_capacity(triangles.len() * 3);
    for t in triangles {
        for i in 0..3 {
            let (a, b) = (t[i], t[(i + 1) % 3]);
            *edges.entry((a, b)).or_insert(0) += 1;
        }
    }
    edges
        .iter()
        .all(|(&(a, b), &count)| count == 1 && edges.get(&(b, a)) == Some(&1))
}

/// `vertices` welded: vertices closer than `distance` become one (the first of them).
/// Returns the vertices kept and, per input vertex, its index among them.
pub fn weld_vertices(vertices: &[Vec3], distance: f32) -> (Vec<Vec3>, Vec<u32>) {
    let cell = f64::from(distance.max(f32::MIN_POSITIVE));
    let key = |p: DVec3| {
        let q = (p / cell).floor();
        (q.x as i64, q.y as i64, q.z as i64)
    };
    let mut grid: HashMap<(i64, i64, i64), Vec<u32>> = HashMap::new();
    let mut out: Vec<Vec3> = Vec::new();
    let mut remap = Vec::with_capacity(vertices.len());
    let limit = f64::from(distance) * f64::from(distance);
    for &v in vertices {
        let p = v.as_dvec3();
        let (x, y, z) = key(p);
        let mut found = None;
        'search: for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    if let Some(list) = grid.get(&(x + dx, y + dy, z + dz)) {
                        for &k in list {
                            if out[k as usize].as_dvec3().distance_squared(p) <= limit {
                                found = Some(k);
                                break 'search;
                            }
                        }
                    }
                }
            }
        }
        let index = found.unwrap_or_else(|| {
            out.push(v);
            let k = (out.len() - 1) as u32;
            grid.entry((x, y, z)).or_default().push(k);
            k
        });
        remap.push(index);
    }
    (out, remap)
}

/// `vertices` welded ([`weld_vertices`]) and the triangles that collapse dropped. Returns
/// the new vertices and triangles.
pub fn weld(
    vertices: &[Vec3],
    triangles: &[[u32; 3]],
    distance: f32,
) -> (Vec<Vec3>, Vec<[u32; 3]>) {
    let (out, remap) = weld_vertices(vertices, distance);
    let triangles = triangles
        .iter()
        .map(|t| t.map(|i| remap[i as usize]))
        .filter(|[a, b, c]| a != b && b != c && a != c)
        .collect();
    (out, triangles)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A unit cube from the origin, closed, counter-clockwise from outside.
    pub(crate) fn cube() -> (Vec<Vec3>, Vec<[u32; 3]>) {
        let vertices = (0..8)
            .map(|i| Vec3::new((i & 1) as f32, ((i >> 1) & 1) as f32, ((i >> 2) & 1) as f32))
            .collect();
        let triangles = vec![
            [0, 2, 1],
            [1, 2, 3],
            [4, 5, 6],
            [5, 7, 6],
            [0, 1, 4],
            [1, 5, 4],
            [2, 6, 3],
            [3, 6, 7],
            [0, 4, 2],
            [2, 4, 6],
            [1, 3, 5],
            [3, 7, 5],
        ];
        (vertices, triangles)
    }

    #[test]
    fn a_cube_is_closed_with_unit_volume() {
        let (v, t) = cube();
        assert!(is_closed(&t));
        assert!((volume(&v, &t) - 1.0).abs() < 1e-6);
        let open = &t[1..];
        assert!(!is_closed(open));
    }

    #[test]
    fn welding_joins_split_seams() {
        let (v, t) = cube();
        // Every triangle with vertices of its own, as render meshes split them.
        let mut split = Vec::new();
        let mut triangles = Vec::new();
        for tri in &t {
            let base = split.len() as u32;
            for &i in tri {
                split.push(v[i as usize] + Vec3::splat(1e-7));
            }
            triangles.push([base, base + 1, base + 2]);
        }
        assert!(!is_closed(&triangles));
        let (welded, triangles) = weld(&split, &triangles, 1e-5);
        assert_eq!(welded.len(), 8);
        assert!(is_closed(&triangles));
        assert!((volume(&welded, &triangles) - 1.0).abs() < 1e-5);
    }
}
