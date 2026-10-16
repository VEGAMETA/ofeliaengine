//! A part's best cutting plane (ADR 0101), after CoACD's Monte Carlo tree search (Wei et al.
//! 2022, MIT). Written for this engine; no code was copied.
//!
//! - **Moves** are planes along the three axes at evenly spaced offsets, shuffled. A state
//!   is a set of parts, each with its volume term; its worst part is the one cut next.
//! - **The tree:** from the root, children are taken by their lower confidence bound (the
//!   search minimizes) until a node with untried moves; one is tried (a cut).
//! - **A rollout** from there cuts the worst part by the best of one plane per axis (by the
//!   volume term) down to the search's depth; its score is the mean over rounds of the worst
//!   part's term. Nodes keep the lowest score seen.
//! - **The result** is the root's best child's plane, refined by a ternary search of its
//!   offset along the best path found (CoACD's `TernaryMCTS`).
//!
//! CoACD's `clip_by_path` compares later parts against the running sum instead of the
//! largest term when it picks the worst part; here the worst part is the one with the
//! largest term.

use super::clip::{Plane, Solid, clip};
use super::cost::volume_term;
use super::map;
use crate::hull::Hull;
use engine_core::jobs::ThreadPool;
use engine_core::math::{DVec3, Vec3};
use std::sync::Arc;

/// How hard to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SearchSettings {
    /// Tree iterations (CoACD's `mcts_iteration`, 150).
    pub(crate) iterations: usize,
    /// Cuts deep (`mcts_max_depth`, 3).
    pub(crate) depth: usize,
    /// Planes per axis (`mcts_nodes`, 20).
    pub(crate) candidates: usize,
    /// Seed of the moves' shuffle.
    pub(crate) seed: u64,
}

/// Clip tolerance in the normalized frame (the mesh's largest side is 2).
pub(crate) const TOLERANCE: f64 = 1e-9;

/// The volume of `solid`'s convex hull.
pub(crate) fn hull_volume(solid: &Solid) -> f64 {
    let points: Vec<Vec3> = solid.points.iter().map(|p| p.as_vec3()).collect();
    Hull::new(&points).map_or(0.0, |h| f64::from(h.volume()))
}

/// A part's volume term.
pub(crate) fn part_term(solid: &Solid) -> f64 {
    volume_term(solid.volume(), hull_volume(solid))
}

/// Planes along each axis across `solid`'s bounds: `count` a side (at least 1 cm apart in
/// the normalized frame, 1.5 cm in from the bounds).
pub(crate) fn axis_planes(solid: &Solid, count: usize) -> Vec<Plane> {
    let (lo, hi) = solid.bounds();
    let mut planes = Vec::new();
    for axis in 0..3 {
        let normal = DVec3::new(
            f64::from(u8::from(axis == 0)),
            f64::from(u8::from(axis == 1)),
            f64::from(u8::from(axis == 2)),
        );
        let interval = ((hi[axis] - lo[axis]) / (count as f64 + 1.0)).max(0.01);
        let margin = interval.max(0.015);
        let mut x = lo[axis] + margin;
        while x <= hi[axis] - margin + 1e-6 {
            planes.push(Plane { normal, offset: x });
            x += interval;
        }
    }
    planes
}

/// `solid` cut by `plane`: both halves with their volume terms; none if it does not cut.
fn halves(solid: &Solid, plane: &Plane) -> Option<[(Solid, f64); 2]> {
    let (pos, neg) = clip(solid, plane, TOLERANCE);
    if pos.triangles.is_empty() || neg.triangles.is_empty() {
        return None;
    }
    let (p, n) = (part_term(&pos), part_term(&neg));
    Some([(pos, p), (neg, n)])
}

/// A deterministic shuffle (xorshift).
fn shuffle(planes: &mut [Plane], seed: u64) {
    let mut state = seed | 1;
    for i in (1..planes.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        planes.swap(i, (state % (i as u64 + 1)) as usize);
    }
}

/// A part of a search state.
#[derive(Clone)]
struct Piece {
    solid: Arc<Solid>,
    cost: f64,
    moves: Arc<Vec<Plane>>,
    next: usize,
}

impl Piece {
    fn new(solid: Solid, cost: f64, settings: &SearchSettings, salt: u64) -> Self {
        let mut moves = axis_planes(&solid, settings.candidates);
        shuffle(&mut moves, settings.seed ^ salt);
        Self {
            solid: Arc::new(solid),
            cost,
            moves: Arc::new(moves),
            next: 0,
        }
    }
}

#[derive(Clone)]
struct State {
    pieces: Vec<Piece>,
    worst: usize,
    round: usize,
    /// The sum over rounds of the worst part's term.
    score: f64,
    /// The plane that made this state (none at the root).
    plane: Option<Plane>,
}

impl State {
    fn terminal(&self, depth: usize) -> bool {
        self.round >= depth || self.pieces[self.worst].moves.is_empty()
    }

    /// The worst part and its term.
    fn update_worst(&mut self) -> f64 {
        let (worst, cost) = self
            .pieces
            .iter()
            .enumerate()
            .fold((0, f64::MIN), |best, (k, p)| {
                if p.cost > best.1 { (k, p.cost) } else { best }
            });
        self.worst = worst;
        cost
    }

    /// This state with its worst part cut by `plane`; none if the plane does not cut it.
    fn cut(&self, plane: Plane, settings: &SearchSettings) -> Option<State> {
        let halves = halves(&self.pieces[self.worst].solid, &plane)?;
        Some(self.split(plane, halves, settings))
    }

    /// This state with its worst part replaced by `halves`, the cut by `plane`.
    fn split(&self, plane: Plane, halves: [(Solid, f64); 2], settings: &SearchSettings) -> State {
        let mut pieces: Vec<Piece> = self
            .pieces
            .iter()
            .enumerate()
            .filter(|&(k, _)| k != self.worst)
            .map(|(_, p)| p.clone())
            .collect();
        let salt = (self.round as u64 + 1) * 0x9E37_79B9;
        for (k, (s, cost)) in halves.into_iter().enumerate() {
            pieces.push(Piece::new(s, cost, settings, salt + k as u64));
        }
        let mut next = State {
            pieces,
            worst: 0,
            round: self.round + 1,
            score: self.score,
            plane: Some(plane),
        };
        next.score += next.update_worst();
        next
    }
}

struct Node {
    state: State,
    children: Vec<usize>,
    parent: Option<usize>,
    visits: u32,
    quality: f64,
}

/// The rollout: cut the worst part by the best middle plane per axis (the one whose worse
/// side has the lowest term; the first of equals) to the depth, the planes tried in
/// parallel. Returns the mean score over the depth and the planes it cut by.
fn rollout(
    state: &State,
    settings: &SearchSettings,
    jobs: Option<&ThreadPool>,
) -> (f64, Vec<Plane>) {
    let mut s = state.clone();
    let mut path = Vec::new();
    while !s.terminal(settings.depth) {
        let worst = Arc::clone(&s.pieces[s.worst].solid);
        let planes = axis_planes(&worst, 1);
        let cuts = map(planes.len(), jobs, |k| halves(&worst, &planes[k]));
        let best = cuts
            .into_iter()
            .enumerate()
            .filter_map(|(k, h)| h.map(|h| (k, h)))
            .min_by(|(_, a), (_, b)| a[0].1.max(a[1].1).total_cmp(&b[0].1.max(b[1].1)));
        let Some((k, halves)) = best else {
            break;
        };
        path.push(planes[k]);
        s = s.split(planes[k], halves, settings);
    }
    (s.score / settings.depth as f64, path)
}

/// The best plane to cut `solid` by, and the path of cuts after it that the search liked
/// best; none if no plane cuts it.
pub(crate) fn search(
    solid: &Solid,
    settings: &SearchSettings,
    jobs: Option<&ThreadPool>,
) -> Option<(Plane, Vec<Plane>)> {
    engine_profiler::zone!("collision::decompose::search");
    let cost = part_term(solid);
    let root = State {
        pieces: vec![Piece::new(solid.clone(), f64::MAX, settings, 0)],
        worst: 0,
        round: 0,
        score: 0.0,
        plane: None,
    };
    // The exploration constant: CoACD's initial term over the depth, over √2.
    let exploration = cost / settings.depth as f64 / 2f64.sqrt();
    let mut nodes = vec![Node {
        state: root,
        children: Vec::new(),
        parent: None,
        visits: 0,
        quality: f64::MAX,
    }];
    let mut best_path: Vec<Plane> = Vec::new();
    let mut best_score = f64::MAX;
    // The root's moves, all at once: until every one is tried, each iteration expands the
    // root's next move whatever the earlier ones found, so these iterations run in parallel
    // and are applied in order, as one at a time would.
    let root_state = nodes[0].state.clone();
    let moves = Arc::clone(&root_state.pieces[0].moves);
    let tried = map(moves.len(), jobs, |k| {
        root_state.cut(moves[k], settings).map(|child| {
            let (score, path) = rollout(&child, settings, jobs);
            (child, score, path)
        })
    });
    let mut done = 0;
    for (k, result) in tried.into_iter().enumerate() {
        if done == settings.iterations {
            break;
        }
        nodes[0].state.pieces[0].next = k + 1;
        let Some((child, score, rollout_path)) = result else {
            continue;
        };
        done += 1;
        let mut path: Vec<Plane> = child.plane.into_iter().collect();
        path.extend(rollout_path);
        nodes.push(Node {
            state: child,
            children: Vec::new(),
            parent: Some(0),
            visits: 1,
            quality: score,
        });
        let c = nodes.len() - 1;
        nodes[0].children.push(c);
        nodes[0].visits += 1;
        nodes[0].quality = nodes[0].quality.min(score);
        if score < best_score {
            best_score = score;
            best_path = path;
        }
    }
    for _ in done..settings.iterations {
        // Down the tree.
        let mut at = 0usize;
        loop {
            let node = &nodes[at];
            if node.state.terminal(settings.depth) {
                break;
            }
            let piece = &node.state.pieces[node.state.worst];
            if piece.next < piece.moves.len() {
                // Expand: the next untried move of the worst part.
                let plane = piece.moves[piece.next];
                let state = node.state.clone();
                let worst = nodes[at].state.worst;
                nodes[at].state.pieces[worst].next += 1;
                if let Some(next) = state.cut(plane, settings) {
                    nodes.push(Node {
                        state: next,
                        children: Vec::new(),
                        parent: Some(at),
                        visits: 0,
                        quality: f64::MAX,
                    });
                    let child = nodes.len() - 1;
                    nodes[at].children.push(child);
                    at = child;
                    break;
                }
                continue;
            }
            // All tried: the child with the lowest bound.
            let parent_visits = f64::from(node.visits.max(1));
            let Some(&child) = node.children.iter().min_by(|&&a, &&b| {
                let bound = |c: usize| {
                    let n = &nodes[c];
                    n.quality
                        - exploration
                            * (2.0 * parent_visits.ln() / f64::from(n.visits.max(1))).sqrt()
                };
                bound(a).total_cmp(&bound(b))
            }) else {
                break;
            };
            at = child;
        }
        if at == 0 && nodes[0].children.is_empty() {
            // Nothing cuts the part.
            return None;
        }
        let (score, rollout_path) = rollout(&nodes[at].state, settings, jobs);
        // Up the tree: the path from the root's child to here, then the rollout's.
        let mut path: Vec<Plane> = Vec::new();
        let mut up = Some(at);
        while let Some(n) = up {
            if let Some(p) = nodes[n].state.plane {
                path.push(p);
            }
            nodes[n].visits += 1;
            nodes[n].quality = nodes[n].quality.min(score);
            up = nodes[n].parent;
        }
        path.reverse();
        path.extend(rollout_path);
        if score < best_score {
            best_score = score;
            best_path = path;
        }
    }
    let first = nodes[0]
        .children
        .iter()
        .min_by(|&&a, &&b| nodes[a].quality.total_cmp(&nodes[b].quality))
        .and_then(|&c| nodes[c].state.plane)?;
    // The best path, if it starts with that plane; otherwise just the plane.
    let rest = if best_path.first() == Some(&first) {
        best_path[1..].to_vec()
    } else {
        Vec::new()
    };
    Some((first, rest))
}

/// The score of cutting `solid` by `first`, then by `path`'s planes on the worst part each
/// time: the mean over the cuts of the worst part's term (infinite if a cut fails).
fn path_score(solid: &Solid, first: Plane, path: &[Plane]) -> f64 {
    let (pos, neg) = clip(solid, &first, TOLERANCE);
    if pos.triangles.is_empty() || neg.triangles.is_empty() {
        return f64::MAX;
    }
    let mut parts: Vec<(Solid, f64)> = [pos, neg]
        .into_iter()
        .map(|s| {
            let c = part_term(&s);
            (s, c)
        })
        .collect();
    let worst = |parts: &[(Solid, f64)]| {
        parts.iter().enumerate().fold(
            (0, f64::MIN),
            |b, (k, p)| if p.1 > b.1 { (k, p.1) } else { b },
        )
    };
    let (mut w, mut total) = worst(&parts);
    for plane in path {
        let (cut, _) = parts.swap_remove(w);
        let (pos, neg) = clip(&cut, plane, TOLERANCE);
        if pos.triangles.is_empty() || neg.triangles.is_empty() {
            return f64::MAX;
        }
        for s in [pos, neg] {
            let c = part_term(&s);
            parts.push((s, c));
        }
        let (k, c) = worst(&parts);
        w = k;
        total += c;
    }
    total / (path.len() + 1) as f64
}

/// `first` moved along its axis to where the path's score is lowest: a ternary search
/// within one candidate spacing of it (CoACD's `TernaryMCTS`, ten steps; each step's two
/// probes in parallel).
pub(crate) fn refine(
    solid: &Solid,
    first: Plane,
    path: &[Plane],
    settings: &SearchSettings,
    jobs: Option<&ThreadPool>,
) -> Plane {
    let (lo, hi) = solid.bounds();
    let axis = (0..3).find(|&a| first.normal[a] > 0.5).unwrap_or(0);
    let interval = ((hi[axis] - lo[axis]) / (settings.candidates as f64 + 1.0)).max(0.01);
    let (mut left, mut right) = (
        (lo[axis] + 0.01).max(first.offset - interval),
        (hi[axis] - 0.01).min(first.offset + interval),
    );
    if left > right {
        return first;
    }
    let at = |x: f64| Plane {
        normal: first.normal,
        offset: x,
    };
    let mut best = (path_score(solid, first, path), first);
    let mut result = first.offset;
    for _ in 0..10 {
        if left + 1e-4 >= right {
            break;
        }
        let third = (right - left) / 3.0;
        let (m1, m2) = (left + third, right - third);
        let probe = |x: f64| path_score(solid, at(x), path);
        let (e1, e2) = match jobs {
            Some(pool) => pool.join(|| probe(m1), || probe(m2)),
            None => (probe(m1), probe(m2)),
        };
        if e1 < e2 {
            right = m2;
            result = m1;
        } else {
            left = m1;
            result = m2;
        }
    }
    let refined = at(result);
    let score = path_score(solid, refined, path);
    if score < best.0 {
        best = (score, refined);
    }
    best.1
}
