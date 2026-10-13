//! Detail levels for meshes (ADR 0053): simplified index lists over a mesh's own vertices,
//! with their geometric error, for `engine_render`'s `GeometryPool::add_mesh_with_lods`.
//! Simplification is meshoptimizer's (through the `meshopt` crate): edge collapses that
//! keep the surface as close as they can, measured by the largest distance the surface
//! moved.

use crate::meshlet::Meshlet;

/// How detail levels are made.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LodSettings {
    /// Most simplified levels (besides the full mesh); the renderer takes at most 3.
    pub levels: usize,
    /// Triangles of each level relative to the previous one.
    pub ratio: f32,
    /// No level below this many triangles.
    pub min_triangles: usize,
    /// Largest error a level may have, relative to the mesh's size (its bounding box's
    /// largest side); beyond it, simplification stops short of the target.
    pub max_error: f32,
}

impl Default for LodSettings {
    /// Up to 3 levels, each half the triangles of the one before, at least 64 triangles,
    /// with errors up to 5% of the mesh's size.
    fn default() -> Self {
        Self {
            levels: 3,
            ratio: 0.5,
            min_triangles: 64,
            max_error: 0.05,
        }
    }
}

/// A simplified level.
#[derive(Debug, Clone, PartialEq)]
pub struct Lod {
    /// Triangles over the mesh's vertices, ordered for the GPU's vertex cache (or meshlet
    /// by meshlet, once the cook has made the level's meshlets).
    pub indices: Vec<u32>,
    /// How far the level's surface strays from the full mesh, in the mesh's units.
    pub error: f32,
    /// The level's meshlets (ADR 0085): consecutive ranges of `indices`, in order; none
    /// until the cook makes them ([`crate::meshlet::build_meshlets`]).
    pub meshlets: Vec<Meshlet>,
}

/// Simplified levels of the mesh `positions` / `indices`, coarser ones last. Fewer than
/// `settings.levels` when the mesh is small or does not simplify further (flat parts
/// already at their fewest triangles, or the error limit reached).
pub fn generate_lods(positions: &[[f32; 3]], indices: &[u32], settings: &LodSettings) -> Vec<Lod> {
    engine_profiler::zone!("assets::generate_lods");
    let full = indices.len() / 3;
    if full == 0 || positions.is_empty() || indices.iter().any(|&i| i as usize >= positions.len()) {
        return Vec::new();
    }
    let scale = meshopt::simplify_scale_decoder(positions);
    let mut lods: Vec<Lod> = Vec::new();
    let mut previous = full;
    for level in 1..=settings.levels.min(3) {
        let target = (full as f32 * settings.ratio.clamp(0.05, 0.95).powi(level as i32)) as usize;
        if target < settings.min_triangles.max(1) {
            break;
        }
        let mut error = 0.0f32;
        // From the full mesh each time: errors measured against the original, not piled up.
        let simplified = meshopt::simplify_decoder(
            indices,
            positions,
            target * 3,
            settings.max_error.max(0.0),
            meshopt::SimplifyOptions::empty(),
            Some(&mut error),
        );
        let triangles = simplified.len() / 3;
        // Stop when simplification no longer pays: under 15% fewer triangles.
        if triangles == 0 || triangles * 100 > previous * 85 {
            break;
        }
        let error = error * scale;
        if lods.last().is_some_and(|l| error < l.error) {
            break;
        }
        lods.push(Lod {
            indices: meshopt::optimize_vertex_cache(&simplified, positions.len()),
            error,
            meshlets: Vec::new(),
        });
        previous = triangles;
    }
    lods
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A UV sphere of `rings` × `segments` quads (two triangles each), radius 1.
    fn sphere(rings: u32, segments: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
        let mut positions = Vec::new();
        for r in 0..=rings {
            let theta = core::f32::consts::PI * r as f32 / rings as f32;
            for s in 0..=segments {
                let phi = core::f32::consts::TAU * s as f32 / segments as f32;
                positions.push([
                    theta.sin() * phi.cos(),
                    theta.cos(),
                    theta.sin() * phi.sin(),
                ]);
            }
        }
        let mut indices = Vec::new();
        let row = segments + 1;
        for r in 0..rings {
            for s in 0..segments {
                let (a, b) = (r * row + s, (r + 1) * row + s);
                indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
            }
        }
        (positions, indices)
    }

    #[test]
    fn levels_halve_the_triangles_with_growing_error() {
        let (positions, indices) = sphere(64, 128);
        let full = indices.len() / 3;
        let lods = generate_lods(&positions, &indices, &LodSettings::default());
        assert_eq!(lods.len(), 3);
        let mut previous = (full, 0.0f32);
        for lod in &lods {
            let triangles = lod.indices.len() / 3;
            assert!(
                triangles * 100 <= previous.0 * 60,
                "{triangles} after {}",
                previous.0
            );
            assert!(
                lod.error >= previous.1,
                "{} after {}",
                lod.error,
                previous.1
            );
            // A unit sphere: the surface moves by far less than its radius.
            assert!(lod.error > 0.0 && lod.error < 0.05, "{}", lod.error);
            assert!(lod.indices.iter().all(|&i| (i as usize) < positions.len()));
            previous = (triangles, lod.error);
        }
    }

    #[test]
    fn small_and_broken_meshes_get_no_levels() {
        let (positions, indices) = sphere(4, 8);
        assert!(generate_lods(&positions, &indices, &LodSettings::default()).is_empty());
        assert!(generate_lods(&[], &[], &LodSettings::default()).is_empty());
        assert!(generate_lods(&positions, &[0, 1, 9_999], &LodSettings::default()).is_empty());
    }
}
