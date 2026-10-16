//! Convex decomposition (ADR 0101, 8c-3): a concave mesh split into convex parts, after
//! CoACD's algorithm (X. Wei, M. Liu, Z. Ling, H. Su, "Approximate Convex Decomposition for 3D
//! Meshes with Collision-Aware Concavity and Tree Search", SIGGRAPH 2022; CoACD is MIT).
//! Ported to Rust for this engine, without CoACD's dependencies: no OpenVDB (our own voxel
//! remesh), no CDT (our own ear clipping), no Boost, and no Sobol sequence (its source is
//! LGPL; Roberts's R2 sequence instead).
//!
//! 1. **Normalize:** the mesh centred, its longest side 2, as CoACD.
//! 2. **A closed manifold:** the mesh itself when it is one, otherwise remeshed
//!    ([`remesh`]); simplified for the search.
//! 3. **Cut:** every part whose concavity ([`cost`]) is above the threshold is cut by the
//!    plane the tree search finds ([`search`]), refined along its axis; the two halves go
//!    back into the queue. The queue's parts are searched in parallel.
//! 4. **Merge** neighbouring parts while the hull of the pair stays within the threshold.
//! 5. **Hulls** of at most `hull_points` points, back in the mesh's frame.

mod bvh;
mod clip;
mod cost;
mod decimate;
mod remesh;
mod search;
mod triangulate;

use self::bvh::Bvh;
use self::clip::{Solid, clip};
use self::search::{SearchSettings, TOLERANCE, refine, search};
use crate::hull::{Hull, HullSettings};
use core::cmp::Ordering;
use engine_core::jobs::ThreadPool;
use engine_core::math::{DVec3, Vec3};
use std::collections::BinaryHeap;
use std::sync::OnceLock;

/// How to decompose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecomposeSettings {
    /// The concavity a part may keep, as a share of the mesh's size (its longest side is 2):
    /// CoACD's 0.05 by default.
    pub threshold: f64,
    /// Tree search iterations per cut (CoACD's 150).
    pub iterations: usize,
    /// Cuts the search looks ahead (CoACD's 3).
    pub depth: usize,
    /// Candidate planes per axis (CoACD's 20).
    pub candidates: usize,
    /// Cells across the mesh's longest side when it has to be remeshed (CoACD's 100).
    pub resolution: usize,
    /// The search works on the mesh simplified to about this many triangles.
    pub search_triangles: usize,
    /// Points sampled on each surface for the Hausdorff term (CoACD's 2,000).
    pub samples: usize,
    /// Merge neighbouring parts afterwards.
    pub merge: bool,
    /// Most points per hull (Jolt takes at most 256).
    pub hull_points: usize,
    /// At most this many parts (each further cut splits the worst part) and hulls (merged
    /// down to it); none: as many as the threshold needs.
    pub max_hulls: Option<usize>,
}

impl Default for DecomposeSettings {
    fn default() -> Self {
        Self {
            threshold: 0.05,
            iterations: 150,
            depth: 3,
            candidates: 20,
            resolution: 100,
            search_triangles: 4_000,
            samples: 2_000,
            merge: true,
            hull_points: 64,
            max_hulls: None,
        }
    }
}

/// What [`decompose`] did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DecomposeStats {
    /// The input was remeshed (it was not a closed manifold).
    pub remeshed: bool,
    /// Triangles the search worked on.
    pub search_triangles: usize,
    /// Cuts made.
    pub cuts: usize,
    /// Parts merged away.
    pub merged: usize,
    /// The largest concavity of a hull kept (a share of the size, as the threshold).
    pub concavity: f64,
    /// Seconds: making the closed, simplified mesh the search works on.
    pub prepare_seconds: f64,
    /// Seconds: searching and cutting.
    pub cut_seconds: f64,
    /// Seconds: merging.
    pub merge_seconds: f64,
}

/// A part: its closed mesh and its concavity.
struct Part {
    solid: Solid,
    concavity: f64,
}

/// What became of a part in a round of cuts.
enum Step {
    /// Cut in two.
    Cut(Solid, Solid),
    /// Kept whole, at this concavity.
    Keep(f64),
}

fn to_f32(points: &[DVec3]) -> Vec<Vec3> {
    points.iter().map(|p| p.as_vec3()).collect()
}

/// The hull of `points` as a solid (empty if they are flat).
fn hull_solid(points: &[DVec3]) -> Solid {
    match Hull::new(&to_f32(points)) {
        Ok(h) => Solid {
            points: h.points.iter().map(|p| p.as_dvec3()).collect(),
            triangles: h.triangles,
        },
        Err(_) => Solid::default(),
    }
}

/// A part's concavity: the larger of its volume term and its boundary Hausdorff distance to
/// its hull.
fn concavity(solid: &Solid, samples: usize) -> f64 {
    let hull = hull_solid(&solid.points);
    if hull.triangles.is_empty() {
        return 0.0;
    }
    let rv = cost::volume_term(solid.volume(), hull.volume());
    let bvh = Bvh::new(&solid.points, &solid.triangles);
    rv.max(cost::hausdorff(&[(solid, &bvh)], &hull, samples))
}

/// The convex parts of the mesh `points` and `triangles`, as point sets (each the points of a
/// hull, in the mesh's frame), with what was done. `jobs` searches parts in parallel.
pub fn decompose(
    points: &[Vec3],
    triangles: &[[u32; 3]],
    settings: &DecomposeSettings,
    jobs: Option<&ThreadPool>,
) -> (Vec<Vec<Vec3>>, DecomposeStats) {
    engine_profiler::zone!("collision::decompose");
    let mut stats = DecomposeStats::default();
    if points.len() < 4 || triangles.is_empty() {
        return (Vec::new(), stats);
    }
    // 1. Normalize.
    let (lo, hi) = points.iter().fold(
        (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
        |(lo, hi), &p| (lo.min(p), hi.max(p)),
    );
    let (lo, hi) = (lo.as_dvec3(), hi.as_dvec3());
    let centre = (lo + hi) * 0.5;
    let scale = 2.0 / (hi - lo).max_element().max(1e-12);
    let normalized: Vec<DVec3> = points
        .iter()
        .map(|p| (p.as_dvec3() - centre) * scale)
        .collect();
    // 2. A closed manifold, simplified for the search.
    let start = std::time::Instant::now();
    let welded = {
        let (w, t) = crate::mesh::weld(&to_f32(&normalized), triangles, 1e-6);
        Solid {
            points: w.iter().map(|p| p.as_dvec3()).collect(),
            triangles: t,
        }
    };
    let solid = if remesh::closed_manifold(&welded) {
        welded
    } else {
        stats.remeshed = true;
        remesh::remesh(&welded.points, &welded.triangles, settings.resolution, 0.55)
    };
    // Within half a remesh cell (a cell is 1 % of the longest side, 0.02 of the normalized
    // frame's 2, at the default resolution).
    let solid = decimate::decimate(&solid, settings.search_triangles, 0.01);
    stats.search_triangles = solid.triangles.len();
    stats.prepare_seconds = start.elapsed().as_secs_f64();
    let start = std::time::Instant::now();
    let search_settings = SearchSettings {
        iterations: settings.iterations,
        depth: settings.depth,
        candidates: settings.candidates,
        seed: 0x00C0_ACD0_5EED,
    };
    // 3. Cut.
    let mut queue = vec![solid];
    let mut done: Vec<Part> = Vec::new();
    let budget = settings.max_hulls.unwrap_or(usize::MAX);
    while !queue.is_empty() {
        let results = map(queue.len(), jobs, |k| {
            let part = &queue[k];
            let c = concavity(part, settings.samples);
            if c > settings.threshold
                && let Some((first, path)) = search(part, &search_settings)
            {
                let plane = refine(part, first, &path, &search_settings);
                let (pos, neg) = clip(part, &plane, TOLERANCE);
                if !pos.triangles.is_empty() && !neg.triangles.is_empty() {
                    return Step::Cut(pos, neg);
                }
            }
            Step::Keep(c)
        });
        let mut next = Vec::new();
        for (part, result) in queue.into_iter().zip(results) {
            match result {
                Step::Cut(pos, neg) if done.len() + next.len() + 2 <= budget.max(2) => {
                    stats.cuts += 1;
                    next.push(pos);
                    next.push(neg);
                }
                Step::Cut(pos, neg) => {
                    // Over the hull budget: keep the halves as they are.
                    for s in [pos, neg] {
                        let c = concavity(&s, settings.samples);
                        done.push(Part {
                            solid: s,
                            concavity: c,
                        });
                    }
                }
                Step::Keep(c) => done.push(Part {
                    solid: part,
                    concavity: c,
                }),
            }
        }
        queue = next;
    }
    stats.cut_seconds = start.elapsed().as_secs_f64();
    // 4. Merge.
    let start = std::time::Instant::now();
    let groups = if settings.merge && done.len() > 1 {
        let (groups, merged) = merge(&done, settings, jobs);
        stats.merged = merged;
        groups
    } else {
        done.into_iter()
            .map(|p| (p.solid.points, p.concavity))
            .collect()
    };
    stats.merge_seconds = start.elapsed().as_secs_f64();
    stats.concavity = groups.iter().map(|g| g.1).fold(0.0, f64::max);
    // 5. Hulls in the mesh's frame.
    let limit = HullSettings {
        max_vertices: settings.hull_points.max(4),
        ..HullSettings::default()
    };
    let hulls = groups
        .iter()
        .filter_map(|(points, _)| {
            let hull = Hull::with_settings(&to_f32(points), &limit).ok()?;
            Some(
                hull.points
                    .iter()
                    .map(|p| (p.as_dvec3() / scale + centre).as_vec3())
                    .collect(),
            )
        })
        .collect();
    (hulls, stats)
}

/// `f(k)` for each `k` in `0..len`, in parallel on `jobs` if given.
fn map<T: Send + Sync>(
    len: usize,
    jobs: Option<&ThreadPool>,
    f: impl Fn(usize) -> T + Sync,
) -> Vec<T> {
    match jobs {
        Some(pool) if len > 1 => {
            let slots: Vec<OnceLock<T>> = (0..len).map(|_| OnceLock::new()).collect();
            pool.parallel_for(len, 1, |_, range| {
                for k in range {
                    let _ = slots[k].set(f(k));
                }
            });
            slots
                .into_iter()
                .enumerate()
                .map(|(k, slot)| slot.into_inner().unwrap_or_else(|| f(k)))
                .collect()
        }
        _ => (0..len).map(f).collect(),
    }
}

/// A part from the cuts, as the merge holds it.
struct Piece<'a> {
    solid: &'a Solid,
    bvh: Bvh,
    lo: DVec3,
    hi: DVec3,
}

/// Parts merged: the pieces they are made of (closed, touching along the cuts between them).
struct Group {
    pieces: Vec<usize>,
    lo: DVec3,
    hi: DVec3,
    volume: f64,
    /// The points of the group's convex hull.
    hull: Vec<DVec3>,
    concavity: f64,
}

/// A pair of groups the merge may join, at the concavity they would have.
struct Pair {
    cost: f64,
    a: u32,
    b: u32,
    stamps: (u32, u32),
}

impl PartialEq for Pair {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Pair {}
impl PartialOrd for Pair {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pair {
    fn cmp(&self, other: &Self) -> Ordering {
        // A max-heap: the cheapest first.
        other
            .cost
            .total_cmp(&self.cost)
            .then(other.a.cmp(&self.a))
            .then(other.b.cmp(&self.b))
    }
}

/// Whether two boxes are farther apart than `limit`.
fn apart(alo: DVec3, ahi: DVec3, blo: DVec3, bhi: DVec3, limit: f64) -> bool {
    (blo - ahi).max(alo - bhi).max(DVec3::ZERO).length() > limit
}

/// Whether two groups come within `limit` of each other: a piece's points against another's
/// triangles, both ways.
fn near(pieces: &[Piece<'_>], a: &Group, b: &Group, limit: f64) -> bool {
    if apart(a.lo, a.hi, b.lo, b.hi, limit) {
        return false;
    }
    let l2 = limit * limit;
    a.pieces.iter().any(|&i| {
        b.pieces.iter().any(|&j| {
            let (p, q) = (&pieces[i], &pieces[j]);
            !apart(p.lo, p.hi, q.lo, q.hi, limit)
                && (p
                    .solid
                    .points
                    .iter()
                    .any(|&x| q.bvh.distance_squared(x, l2) < l2)
                    || q.solid
                        .points
                        .iter()
                        .any(|&x| p.bvh.distance_squared(x, l2) < l2))
        })
    })
}

/// Two groups joined, with the concavity of the result.
fn joined(pieces: &[Piece<'_>], a: &Group, b: &Group, samples: usize) -> Group {
    let mut points = a.hull.clone();
    points.extend_from_slice(&b.hull);
    let hull = hull_solid(&points);
    let volume = a.volume + b.volume;
    let concavity = if hull.triangles.is_empty() {
        0.0
    } else {
        let all: Vec<(&Solid, &Bvh)> = a
            .pieces
            .iter()
            .chain(&b.pieces)
            .map(|&k| (pieces[k].solid, &pieces[k].bvh))
            .collect();
        cost::volume_term(volume, hull.volume()).max(cost::hausdorff(&all, &hull, samples))
    };
    Group {
        pieces: a.pieces.iter().chain(&b.pieces).copied().collect(),
        lo: a.lo.min(b.lo),
        hi: a.hi.max(b.hi),
        volume,
        hull: if hull.points.is_empty() {
            points
        } else {
            hull.points
        },
        concavity,
    }
}

/// Merges pairs of parts, the cheapest first, while the merged part's concavity is within the
/// threshold (or down to `max_hulls`), as CoACD's cost matrix does: every pair's cost once,
/// then only the new part's against the rest after each merge. Pairs farther apart than the
/// threshold are not considered unless that is the only way down to `max_hulls`. Returns each
/// result's points (its hull's) and concavity, and how many parts were merged away.
fn merge(
    parts: &[Part],
    settings: &DecomposeSettings,
    jobs: Option<&ThreadPool>,
) -> (Vec<(Vec<DVec3>, f64)>, usize) {
    engine_profiler::zone!("collision::decompose::merge");
    let n = parts.len();
    let pieces: Vec<Piece<'_>> = map(n, jobs, |k| {
        let solid = &parts[k].solid;
        let (lo, hi) = solid.bounds();
        Piece {
            solid,
            bvh: Bvh::new(&solid.points, &solid.triangles),
            lo,
            hi,
        }
    });
    let mut groups: Vec<Option<Group>> = map(n, jobs, |k| {
        let p = &pieces[k];
        let hull = hull_solid(&p.solid.points).points;
        Some(Group {
            pieces: vec![k],
            lo: p.lo,
            hi: p.hi,
            volume: p.solid.volume(),
            hull: if hull.is_empty() {
                p.solid.points.clone()
            } else {
                hull
            },
            concavity: parts[k].concavity,
        })
    });
    let mut stamps = vec![0u32; n];
    let mut heap = BinaryHeap::new();
    let evaluate = |pairs: &[(usize, usize)],
                    groups: &[Option<Group>],
                    stamps: &[u32],
                    anywhere: bool,
                    heap: &mut BinaryHeap<Pair>| {
        let costs = map(pairs.len(), jobs, |k| {
            let (a, b) = pairs[k];
            let (Some(ga), Some(gb)) = (&groups[a], &groups[b]) else {
                return None;
            };
            (anywhere || near(&pieces, ga, gb, settings.threshold))
                .then(|| joined(&pieces, ga, gb, settings.samples).concavity)
        });
        for (&(a, b), cost) in pairs.iter().zip(costs) {
            if let Some(cost) = cost {
                heap.push(Pair {
                    cost,
                    a: a as u32,
                    b: b as u32,
                    stamps: (stamps[a], stamps[b]),
                });
            }
        }
    };
    let all: Vec<(usize, usize)> = (0..n)
        .flat_map(|a| (a + 1..n).map(move |b| (a, b)))
        .collect();
    evaluate(&all, &groups, &stamps, false, &mut heap);
    // Pairs however far apart, once that is the only way down to max_hulls.
    let mut anywhere = false;
    let mut count = n;
    loop {
        let over = settings.max_hulls.is_some_and(|m| count > m);
        let pair = loop {
            match heap.pop() {
                Some(p)
                    if groups[p.a as usize].is_some()
                        && groups[p.b as usize].is_some()
                        && p.stamps == (stamps[p.a as usize], stamps[p.b as usize]) =>
                {
                    break Some(p);
                }
                Some(_) => {}
                None => break None,
            }
        };
        let Some(pair) = pair else {
            if over && !anywhere {
                anywhere = true;
                let live: Vec<usize> = (0..n).filter(|&k| groups[k].is_some()).collect();
                let pairs: Vec<(usize, usize)> = live
                    .iter()
                    .enumerate()
                    .flat_map(|(i, &a)| live[i + 1..].iter().map(move |&b| (a, b)))
                    .collect();
                evaluate(&pairs, &groups, &stamps, anywhere, &mut heap);
                continue;
            }
            break;
        };
        if pair.cost > settings.threshold && !over {
            break;
        }
        let (a, b) = (pair.a as usize, pair.b as usize);
        let (Some(ga), Some(gb)) = (groups[a].take(), groups[b].take()) else {
            break;
        };
        let mut g = joined(&pieces, &ga, &gb, settings.samples);
        g.concavity = pair.cost;
        groups[a] = Some(g);
        stamps[a] += 1;
        count -= 1;
        let row: Vec<(usize, usize)> = (0..n)
            .filter(|&k| k != a && groups[k].is_some())
            .map(|k| (k.min(a), k.max(a)))
            .collect();
        evaluate(&row, &groups, &stamps, anywhere, &mut heap);
    }
    let out = groups
        .into_iter()
        .flatten()
        .map(|g| (g.hull, g.concavity))
        .collect();
    (out, n - count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prism: the counter-clockwise polygon `outline` (in x, z) from y = 0 to `height`.
    fn prism(outline: &[(f32, f32)], height: f32) -> (Vec<Vec3>, Vec<[u32; 3]>) {
        let n = outline.len() as u32;
        let mut points: Vec<Vec3> = outline.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
        points.extend(outline.iter().map(|&(x, z)| Vec3::new(x, height, z)));
        let flat: Vec<engine_core::math::DVec2> = outline
            .iter()
            .map(|&(x, z)| engine_core::math::DVec2::new(f64::from(x), f64::from(z)))
            .collect();
        let ring: Vec<u32> = (0..n).collect();
        let mut caps = Vec::new();
        triangulate::triangulate(&flat, &ring, &mut [], &mut caps);
        let mut triangles = Vec::new();
        // The outline is counter-clockwise seen from +y in (x, z)... seen from above (+y),
        // (x, z) is a left-handed view, so the top cap keeps the 2D winding reversed.
        for &[a, b, c] in &caps {
            triangles.push([a, b, c]);
            triangles.push([a + n, c + n, b + n]);
        }
        for k in 0..n {
            let k1 = (k + 1) % n;
            triangles.push([k, k + n, k1]);
            triangles.push([k1, k + n, k1 + n]);
        }
        (points, triangles)
    }

    fn outward(points: &[Vec3], triangles: &mut [[u32; 3]]) {
        let v = crate::mesh::volume(points, triangles);
        if v < 0.0 {
            for t in triangles.iter_mut() {
                t.swap(1, 2);
            }
        }
    }

    #[test]
    fn a_box_stays_whole_and_an_l_splits_in_two() {
        let (points, mut triangles) = prism(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)], 1.0);
        outward(&points, &mut triangles);
        assert!(crate::mesh::is_closed(&triangles));
        let (hulls, stats) = decompose(&points, &triangles, &DecomposeSettings::default(), None);
        assert_eq!(hulls.len(), 1, "{stats:?}");
        assert!(!stats.remeshed);
        // An L: two arms of 2 × 0.5.
        let l = [
            (0.0, 0.0),
            (2.0, 0.0),
            (2.0, 0.5),
            (0.5, 0.5),
            (0.5, 2.0),
            (0.0, 2.0),
        ];
        let (points, mut triangles) = prism(&l, 0.5);
        outward(&points, &mut triangles);
        assert!(crate::mesh::is_closed(&triangles));
        let (hulls, stats) = decompose(&points, &triangles, &DecomposeSettings::default(), None);
        assert_eq!(hulls.len(), 2, "{stats:?}");
        assert!(stats.concavity <= 0.05, "{stats:?}");
        // The hulls' volumes add up to the L's.
        let total: f32 = hulls.iter().map(|h| Hull::new(h).unwrap().volume()).sum();
        assert!((total - 1.75 * 0.5).abs() < 0.01, "{total}");
    }

    #[test]
    fn a_u_splits_in_three_in_parallel() {
        let u = [
            (0.0, 0.0),
            (3.0, 0.0),
            (3.0, 2.0),
            (2.0, 2.0),
            (2.0, 1.0),
            (1.0, 1.0),
            (1.0, 2.0),
            (0.0, 2.0),
        ];
        let (points, mut triangles) = prism(&u, 1.0);
        outward(&points, &mut triangles);
        let jobs = ThreadPool::builder().workers(4).build().unwrap();
        let (hulls, stats) = decompose(
            &points,
            &triangles,
            &DecomposeSettings::default(),
            Some(&jobs),
        );
        assert_eq!(hulls.len(), 3, "{stats:?}");
        let total: f32 = hulls.iter().map(|h| Hull::new(h).unwrap().volume()).sum();
        assert!((total - 5.0).abs() < 0.05, "{total}");
    }
}
