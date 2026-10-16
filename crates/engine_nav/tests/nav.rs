//! Navigation meshes baked from simple levels with known answers: around a wall, across
//! tiles, to an island it cannot reach (ADR 0103).

#![allow(clippy::unwrap_used)]

use engine_core::jobs::ThreadPool;
use engine_core::math::Vec3;
use engine_nav::{BakeSettings, NavMesh, PathStatus, Raycast, bake};

/// A level being built: vertices and triangles.
#[derive(Default)]
struct Level {
    vertices: Vec<Vec3>,
    triangles: Vec<[u32; 3]>,
}

impl Level {
    /// A horizontal rectangle at height `y`, facing up.
    fn floor(&mut self, x: [f32; 2], z: [f32; 2], y: f32) {
        let base = self.vertices.len() as u32;
        self.vertices.extend([
            Vec3::new(x[0], y, z[0]),
            Vec3::new(x[0], y, z[1]),
            Vec3::new(x[1], y, z[1]),
            Vec3::new(x[1], y, z[0]),
        ]);
        self.triangles
            .extend([[base, base + 1, base + 2], [base, base + 2, base + 3]]);
    }

    /// A box: its top, facing up, and its four sides.
    fn block(&mut self, lo: Vec3, hi: Vec3) {
        self.floor([lo.x, hi.x], [lo.z, hi.z], hi.y);
        let corner = |x: f32, y: f32, z: f32| Vec3::new(x, y, z);
        let sides = [
            [
                corner(lo.x, lo.y, lo.z),
                corner(hi.x, lo.y, lo.z),
                corner(hi.x, hi.y, lo.z),
                corner(lo.x, hi.y, lo.z),
            ],
            [
                corner(hi.x, lo.y, hi.z),
                corner(lo.x, lo.y, hi.z),
                corner(lo.x, hi.y, hi.z),
                corner(hi.x, hi.y, hi.z),
            ],
            [
                corner(lo.x, lo.y, hi.z),
                corner(lo.x, lo.y, lo.z),
                corner(lo.x, hi.y, lo.z),
                corner(lo.x, hi.y, hi.z),
            ],
            [
                corner(hi.x, lo.y, lo.z),
                corner(hi.x, lo.y, hi.z),
                corner(hi.x, hi.y, hi.z),
                corner(hi.x, hi.y, lo.z),
            ],
        ];
        for quad in sides {
            let base = self.vertices.len() as u32;
            self.vertices.extend(quad);
            self.triangles
                .extend([[base, base + 1, base + 2], [base, base + 2, base + 3]]);
        }
    }

    fn mesh(&self, jobs: Option<&ThreadPool>) -> NavMesh {
        let data = bake(
            &self.vertices,
            &self.triangles,
            &BakeSettings::default(),
            jobs,
        )
        .unwrap();
        NavMesh::new(&data).unwrap()
    }
}

const EXTENT: Vec3 = Vec3::new(1.0, 2.0, 1.0);

fn length(path: &[Vec3]) -> f32 {
    path.windows(2).map(|w| w[0].distance(w[1])).sum()
}

#[test]
fn a_path_goes_around_a_wall_through_its_gap() {
    // A 20 m floor; a wall across it from z = -10 to 6, so the way round is past z = 6.
    let mut level = Level::default();
    level.floor([-10.0, 10.0], [-10.0, 10.0], 0.0);
    level.block(Vec3::new(-0.5, 0.0, -10.0), Vec3::new(0.5, 3.0, 6.0));
    let mut nav = level.mesh(None);
    let (from, to) = (Vec3::new(-5.0, 0.0, -5.0), Vec3::new(5.0, 0.0, -5.0));
    let mut path = Vec::new();
    assert_eq!(nav.path(from, to, EXTENT, &mut path), PathStatus::Complete);
    assert!(path.len() >= 3, "it turns: {path:?}");
    assert!(path[0].distance(from) < 0.2 && path.last().unwrap().distance(to) < 0.2);
    // Round the wall's end, the agent's radius clear of it.
    assert!(path.iter().any(|p| p.z > 6.3), "{path:?}");
    // Up 11 m to the gap and back down, about 2 × hypot(5, 11.4).
    let walked = length(&path);
    assert!(walked > 24.0 && walked < 26.5, "{walked} m: {path:?}");
    // Straight across, the wall stops it about halfway.
    match nav.raycast(from, to, EXTENT) {
        Raycast::Blocked(t) => assert!((0.4..0.5).contains(&t), "{t}"),
        other => panic!("{other:?}"),
    }
    // Along the wall nothing is in the way.
    assert_eq!(
        nav.raycast(from, Vec3::new(-5.0, 0.0, 5.0), EXTENT),
        Raycast::Reached
    );
    // Above the floor: the floor below.
    let p = nav.nearest(Vec3::new(-3.0, 1.5, 2.0), EXTENT).unwrap();
    assert!(p.distance(Vec3::new(-3.0, 0.0, 2.0)) < 0.2, "{p}");
}

#[test]
fn paths_cross_tiles_baked_in_parallel() {
    // 40 m across: tiles of 12 m, 4 × 4 of them.
    let mut level = Level::default();
    level.floor([-20.0, 20.0], [-20.0, 20.0], 0.0);
    let jobs = ThreadPool::builder().workers(3).build().unwrap();
    let data = bake(
        &level.vertices,
        &level.triangles,
        &BakeSettings::default(),
        Some(&jobs),
    )
    .unwrap();
    assert_eq!(data.tiles.len(), 16);
    let mut nav = NavMesh::new(&data).unwrap();
    let mut path = Vec::new();
    let (from, to) = (Vec3::new(-18.0, 0.0, -18.0), Vec3::new(18.0, 0.0, 17.0));
    assert_eq!(nav.path(from, to, EXTENT, &mut path), PathStatus::Complete);
    // Open ground: straight, across the tiles' seams.
    let straight = from.distance(to);
    assert!(
        (length(&path) - straight).abs() < 0.02 * straight,
        "{path:?}"
    );
}

#[test]
fn an_island_out_of_reach_gives_a_partial_path_to_the_shore_facing_it() {
    let mut level = Level::default();
    level.floor([-10.0, 10.0], [-10.0, 10.0], 0.0);
    level.floor([30.0, 34.0], [-2.0, 2.0], 0.0);
    let mut nav = level.mesh(None);
    let mut path = Vec::new();
    let status = nav.path(Vec3::ZERO, Vec3::new(32.0, 0.0, 0.0), EXTENT, &mut path);
    assert_eq!(status, PathStatus::Partial);
    // On the shore that faces it (Detour stops in the polygon its search found nearest,
    // not at the exact nearest point).
    let end = *path.last().unwrap();
    assert!(end.x > 9.0 && end.x < 10.0 && end.z.abs() < 9.6, "{end}");
    // Off the mesh altogether: nothing.
    assert_eq!(
        nav.path(Vec3::new(0.0, 50.0, 0.0), Vec3::ZERO, EXTENT, &mut path),
        PathStatus::None
    );
    assert!(path.is_empty());
}

#[test]
fn nothing_walkable_bakes_to_no_tiles() {
    let data = bake(&[], &[], &BakeSettings::default(), None).unwrap();
    assert!(data.tiles.is_empty());
    let mut level = Level::default();
    // A wall only: nothing to stand on but its top, too small for the agent.
    level.block(Vec3::new(0.0, 0.0, 0.0), Vec3::new(0.2, 3.0, 5.0));
    let data = bake(
        &level.vertices,
        &level.triangles,
        &BakeSettings::default(),
        None,
    )
    .unwrap();
    assert!(data.tiles.is_empty(), "{} tiles", data.tiles.len());
}
