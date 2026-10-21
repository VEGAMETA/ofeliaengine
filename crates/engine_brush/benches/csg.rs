//! Brush CSG (ADR 0110): what an exact side costs, and a greybox of rooms evaluated from
//! nothing and around one room (what an edit redoes); then hollow rooms joined by
//! corridors that open into them. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_brush --bench csg
//! ```

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_brush::csg::surface;
use engine_brush::shape::{box_planes, cuboid, grown};
use engine_brush::{Brush, Group, Op, Plane, Planes, Point, evaluate};
use engine_core::math::DVec3;
use std::hint::black_box;
use std::time::Instant;

struct Random(u64);

impl Random {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn unit(&mut self) -> DVec3 {
        DVec3::new(self.next() - 0.5, self.next() - 0.5, self.next() - 0.5).normalize()
    }

    fn grid(&mut self, half: i64) -> [i64; 3] {
        [0; 3].map(|_| ((self.next() * 2.0 - 1.0) * half as f64) as i64)
    }
}

/// Median of `runs` timings, in microseconds.
fn median_us<R>(runs: usize, mut f: impl FnMut() -> R) -> f64 {
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            black_box(f());
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[runs / 2]
}

/// Rooms of 10 × 8 m inside, walls 0.2 m, 3 m high, every 12 m along x and 10 m along z,
/// on one ground: each a box, its inside cut, a door, a window and a pillar (five brushes).
fn rooms(n: i64, table: &mut Planes) -> Vec<Brush> {
    let mm = |v: f64| (v * 1000.0) as i64;
    let mut out = Vec::new();
    let mut add = |op: Op, lo: [f64; 3], hi: [f64; 3], table: &mut Planes| {
        let planes = box_planes(lo.map(mm), hi.map(mm), table).unwrap();
        out.push(Brush {
            op,
            parts: vec![planes],
            inside: Vec::new(),
        });
    };
    let (w, d) = (12.0 * n as f64, 10.0 * n as f64);
    add(Op::Add, [-1.0, -0.5, -1.0], [w + 1.0, 0.0, d + 1.0], table);
    for i in 0..n {
        for j in 0..n {
            let (x, z) = (12.0 * i as f64, 10.0 * j as f64);
            add(Op::Add, [x, 0.0, z], [x + 10.4, 3.2, z + 8.4], table);
            add(
                Op::Cut,
                [x + 0.2, 0.2, z + 0.2],
                [x + 10.2, 3.0, z + 8.2],
                table,
            );
            add(
                Op::Cut,
                [x + 10.1, 0.2, z + 3.7],
                [x + 10.5, 2.3, z + 4.7],
                table,
            );
            add(
                Op::Cut,
                [x + 4.0, 1.2, z + 8.1],
                [x + 5.2, 2.2, z + 8.5],
                table,
            );
            add(
                Op::Add,
                [x + 5.0, 0.2, z + 4.0],
                [x + 5.4, 3.0, z + 4.4],
                table,
            );
        }
    }
    out
}

/// Hollow rooms (team decision, 2026-10-21): 10 × 8 m inside, 3 m high, walls 0.2 m, every
/// 12 m along x and 10 m along z on one ground, and from each room a corridor 2 m wide into
/// its neighbour along x, no doorway cut: their insides open the walls.
fn hollow_rooms(n: i64, table: &mut Planes) -> Vec<Brush> {
    let mm = |v: f64| (v * 1000.0) as i64;
    let mut out = Vec::new();
    let (w, d) = (12.0 * n as f64, 10.0 * n as f64);
    out.push(Brush {
        op: Op::Add,
        parts: vec![
            box_planes(
                [-1.0, -0.5, -1.0].map(mm),
                [w + 1.0, -0.2, d + 1.0].map(mm),
                table,
            )
            .unwrap(),
        ],
        inside: Vec::new(),
    });
    let mut room = |lo: [f64; 3], hi: [f64; 3], table: &mut Planes| {
        let inside = cuboid(lo.map(mm), hi.map(mm)).unwrap();
        let walls = grown(std::slice::from_ref(&inside), 200).unwrap();
        out.push(Brush {
            op: Op::Add,
            parts: walls
                .into_iter()
                .map(|p| p.into_iter().map(|q| table.add(q)).collect())
                .collect(),
            inside: vec![inside.into_iter().map(|q| table.add(q)).collect()],
        });
    };
    for i in 0..n {
        for j in 0..n {
            let (x, z) = (12.0 * i as f64, 10.0 * j as f64);
            room([x, 0.0, z], [x + 10.0, 3.0, z + 8.0], table);
            if i + 1 < n {
                room([x + 9.0, 0.0, z + 3.0], [x + 13.0, 2.4, z + 5.0], table);
            }
        }
    }
    out
}

/// `brush` moved `dx` grid units along x, its inside too.
fn moved_x(group: &mut Group, brush: &Brush, dx: i64) -> Brush {
    let mut shift = |parts: &[Vec<engine_brush::PlaneId>]| -> Vec<Vec<engine_brush::PlaneId>> {
        parts
            .iter()
            .map(|part| {
                part.iter()
                    .map(|&id| {
                        let p = *group.planes().get(id);
                        let q = Plane::new(p.n.map(i64::from), p.d + i64::from(p.n[0]) * dx);
                        group.planes_mut().add(q.unwrap())
                    })
                    .collect()
            })
            .collect()
    };
    Brush {
        op: brush.op,
        parts: shift(&brush.parts),
        inside: shift(&brush.inside),
    }
}

fn main() {
    // An exact side: random planes through grid points within ±10 km, and points where
    // three of them meet.
    let mut rng = Random(0x9e37_79b9_7f4a_7c15);
    let planes: Vec<Plane> = (0..3 * 1024)
        .map(|_| Plane::through(rng.unit(), rng.grid(10_000_000)).unwrap())
        .collect();
    let points: Vec<Point> = planes
        .chunks(3)
        .filter_map(|p| Point::meet(&p[0], &p[1], &p[2]))
        .collect();
    let tests = planes.len() * points.len();
    let side_ns = median_us(9, || {
        let mut behind = 0u32;
        for p in &points {
            for s in &planes {
                behind += u32::from(p.side(s).is_lt());
            }
        }
        behind
    }) * 1e3
        / tests as f64;
    let meet_ns = median_us(9, || {
        planes
            .chunks(3)
            .filter_map(|p| Point::meet(&p[0], &p[1], &p[2]))
            .count()
    }) * 1e3
        / (planes.len() / 3) as f64;
    println!("| Operation | ns |");
    println!("|---|---|");
    println!("| a point's side of a plane | {side_ns:.2} |");
    println!("| the point where three planes meet | {meet_ns:.2} |");
    println!();

    println!(
        "| Rooms | Brushes | Pieces | Surface polygons | Triangles | Evaluated (ms) | Of it the surface (ms) |"
    );
    println!("|---|---|---|---|---|---|---|");
    for n in [1i64, 3, 10, 20] {
        let mut table = Planes::new();
        let brushes = rooms(n, &mut table);
        let result = evaluate(&brushes, &table);
        assert!(result.invalid.is_empty());
        let triangles: usize = result
            .surface
            .iter()
            .map(|(_, p)| p.corners.len() - 2)
            .sum();
        let runs = if n >= 20 { 3 } else { 9 };
        let ms = median_us(runs, || evaluate(&brushes, &table).surface.len()) / 1e3;
        let surface_ms = median_us(runs, || surface(&result.pieces, &table).len()) / 1e3;
        println!(
            "| {} | {} | {} | {} | {} | {:.2} | {:.2} |",
            n * n,
            brushes.len(),
            result.pieces.len(),
            result.surface.len(),
            triangles,
            ms,
            surface_ms
        );
    }

    // An edit: one brush dragged a millimetre along x and back, each move redoing what it
    // touches in a group kept evaluated.
    println!();
    println!("| 400 rooms (2,001 brushes) | ms |");
    println!("|---|---|");
    let mut group = Group::new();
    let brushes = rooms(20, group.planes_mut());
    let rebuild_ms = median_us(3, || {
        let mut g = Group::new();
        *g.planes_mut() = group.planes().clone();
        g.rebuild(brushes.clone());
        g.pieces().count()
    }) / 1e3;
    group.rebuild(brushes);
    println!("| rebuilt from nothing (tiles kept) | {rebuild_ms:.2} |");
    // Room (10, 10): its box, inside, door, window, pillar.
    let room = 1 + 5 * (10 * 20 + 10);
    let mut polygons = Vec::new();
    for (what, index) in [
        ("a door (a cut) dragged", room + 2),
        ("a room's box (an add) dragged", room),
        ("a pillar (an add inside) dragged", room + 4),
    ] {
        let start: Vec<Plane> = group.brushes()[index].parts[0]
            .iter()
            .map(|&id| *group.planes().get(id))
            .collect();
        let mut step = 0i64;
        let mut redone = Default::default();
        let ms = median_us(31, || {
            step = 1 - step;
            let moved = start
                .iter()
                .map(|p| {
                    let d = p.d + i64::from(p.n[0]) * step;
                    group
                        .planes_mut()
                        .add(Plane::new(p.n.map(i64::from), d).unwrap())
                })
                .collect();
            let op = group.brushes()[index].op;
            redone = group.set(
                index,
                Brush {
                    op,
                    parts: vec![moved],
                    inside: Vec::new(),
                },
            );
            // What the meshes would be made from.
            let mut n = 0;
            for b in group.take_changed() {
                group.polygons_of(b as usize, &mut polygons);
                n += polygons.len();
            }
            n
        }) / 1e3;
        println!(
            "| {what}: {} brushes and {} tiles redone, changed polygons gathered | {ms:.3} |",
            redone.brushes, redone.tiles
        );
    }

    // Hollow rooms and their corridors: from nothing, and a corridor and a room dragged a
    // millimetre along x and back.
    println!();
    let mut group = Group::new();
    let brushes = hollow_rooms(20, group.planes_mut());
    let count = brushes.len();
    println!("| 400 hollow rooms, 380 corridors ({count} brushes) | ms |");
    println!("|---|---|");
    let fresh_ms = median_us(3, || evaluate(&brushes, group.planes()).surface.len()) / 1e3;
    println!("| evaluated from nothing | {fresh_ms:.2} |");
    let rebuild_ms = median_us(3, || {
        let mut g = Group::new();
        *g.planes_mut() = group.planes().clone();
        g.rebuild(brushes.clone());
        g.pieces().count()
    }) / 1e3;
    group.rebuild(brushes);
    println!("| rebuilt from nothing (tiles kept) | {rebuild_ms:.2} |");
    // Room (10, 10) and the corridor from it: a room is brush 1 + 40 i + 2 j (each column
    // of rooms but the last has a corridor per room).
    let room = 1 + 40 * 10 + 2 * 10;
    let mut polygons = Vec::new();
    for (what, index) in [("a corridor dragged", room + 1), ("a room dragged", room)] {
        let start = group.brushes()[index].clone();
        let mut step = 0i64;
        let mut redone = Default::default();
        let ms = median_us(31, || {
            step = 1 - step;
            let b = moved_x(&mut group, &start, step);
            redone = group.set(index, b);
            let mut n = 0;
            for b in group.take_changed() {
                group.polygons_of(b as usize, &mut polygons);
                n += polygons.len();
            }
            n
        }) / 1e3;
        println!(
            "| {what}: {} brushes and {} tiles redone, changed polygons gathered | {ms:.3} |",
            redone.brushes, redone.tiles
        );
    }
}
