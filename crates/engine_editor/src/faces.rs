//! Editing a brush's faces (ADR 0110 E2b-5; `docs/research/ux/blockout.md` §5): with Tab
//! on the selected brushes, the face under the pointer lights; a drag moves its plane along
//! its normal, exactly; Alt+drag pulls a new brush out of it, or, pushed in, a cut of its
//! outline.
//!
//! - **An edited brush keeps its planes** (`BrushShape::Planes`): its shape's parameters
//!   go, and undo brings them back. A room keeps its walls: what moves is its inside's
//!   plane, the wall following.
//! - **A move that would leave a part of the brush empty stops** before it.
//! - **Corners and edges** (E2b-5b): a drag moves a corner, or an edge's two, in the plane
//!   of the brush's own axes that faces the camera most, and the part is made again as the
//!   hull of its corners (`engine_brush::shape::hull`): corners that meet merge, so an edge
//!   moved onto another makes a wedge; a corner dragged inside goes. A room's corners are its
//!   walls', not moved: its faces are.
//! - **The owner's snapping rules** ([`crate::Snapping`]): the distance in whole steps of the
//!   position step with snapping on (G), or Shift with it off; Ctrl rounds it to the step.
//!   Free, it lands on the brushes' millimetre grid.

use crate::gizmo::{ViewProj, hit_plane};
use crate::snap::{Quantity, Snapping};
use engine_app::{Brush, BrushOp, BrushPlane, BrushShape};
use engine_brush::{Owner, Plane, PlaneId, Planes, Solid};
use engine_core::math::{Affine3A, DVec3, Mat3, Vec2, Vec3};

/// A brush's grid, metres.
const UNIT: f64 = 0.001;

/// One face of a brush: its part and its plane's place among the part's planes, its
/// corners (counter-clockwise from in front) and its outward normal, in the world.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Face {
    pub(crate) part: usize,
    pub(crate) plane: usize,
    pub(crate) corners: Vec<Vec3>,
    pub(crate) normal: Vec3,
}

impl Face {
    /// Its corners' middle.
    pub(crate) fn middle(&self) -> Vec3 {
        self.corners.iter().copied().sum::<Vec3>() / self.corners.len().max(1) as f32
    }
}

/// The planes an edit moves, in the brush's own grid: a room's inside's (its walls grow
/// from them), any other brush's own.
pub(crate) fn base_parts(brush: &Brush) -> Vec<Vec<Plane>> {
    if brush.is_room() {
        brush.inside()
    } else {
        brush.parts()
    }
}

/// The faces of `brush` placed at `world` (its global transform), into `out` (cleared
/// first): each part's solid from its planes, what shows of a room its walls' outside.
pub(crate) fn faces(brush: &Brush, world: Affine3A, out: &mut Vec<Face>) {
    out.clear();
    let normals = Mat3::from(world.matrix3).inverse().transpose();
    let mut table = Planes::new();
    for (p, part) in brush.parts().iter().enumerate() {
        let Some(solid) = solid(part, &mut table) else {
            continue;
        };
        for f in &solid.faces {
            let n = table.get(f.polygon.on).n;
            let normal = normals * Vec3::new(n[0] as f32, n[1] as f32, n[2] as f32);
            out.push(Face {
                part: p,
                plane: f.owner.face as usize,
                corners: f
                    .polygon
                    .corners
                    .iter()
                    .map(|c| world.transform_point3((c.to_f64() * UNIT).as_vec3()))
                    .collect(),
                normal: normal.normalize_or_zero(),
            });
        }
    }
}

/// The solid of a part's planes, its faces owned by their places among them.
fn solid(part: &[Plane], table: &mut Planes) -> Option<Solid> {
    let planes: Vec<(PlaneId, Owner)> = part
        .iter()
        .enumerate()
        .map(|(k, &q)| {
            (
                table.add(q),
                Owner {
                    brush: 0,
                    face: k as u32,
                },
            )
        })
        .collect();
    Solid::from_planes(&planes, table).ok()
}

/// The face the ray `(origin, direction)` meets first and where along it: the nearest face
/// turned towards it, or, from inside the brush, the nearest it leaves through.
pub(crate) fn pick(faces: &[Face], (o, d): (Vec3, Vec3)) -> Option<(usize, f32)> {
    let hit = |front: bool| {
        let mut best: Option<(usize, f32)> = None;
        for (i, f) in faces.iter().enumerate() {
            let along = f.normal.dot(d);
            if (front && along >= -1e-6) || (!front && along <= 1e-6) {
                continue;
            }
            let t = (f.corners[0] - o).dot(f.normal) / along;
            if t < 0.0 || best.is_some_and(|(_, b)| t >= b) {
                continue;
            }
            let p = o + d * t;
            let inside = (0..f.corners.len()).all(|k| {
                let (a, b) = (f.corners[k], f.corners[(k + 1) % f.corners.len()]);
                (b - a).cross(p - a).dot(f.normal) >= -1e-5
            });
            if inside {
                best = Some((i, t));
            }
        }
        best
    };
    hit(true).or_else(|| hit(false))
}

/// A part's corners on the brushes' millimetre grid (each once) and its edges between them,
/// for picking and moving them: the part's place among the brush's, each corner in the
/// grid and in the world.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PartCorners {
    pub(crate) part: usize,
    pub(crate) grid: Vec<[i64; 3]>,
    pub(crate) world: Vec<Vec3>,
    pub(crate) edges: Vec<(usize, usize)>,
}

/// The corners and edges of each part of `brush` placed at `world`, into `out` (cleared
/// first); none for a room (its faces are edited, its corners are its walls').
pub(crate) fn corners(brush: &Brush, world: Affine3A, out: &mut Vec<PartCorners>) {
    out.clear();
    if brush.is_room() {
        return;
    }
    let mut table = Planes::new();
    for (p, part) in brush.parts().iter().enumerate() {
        let Some(solid) = solid(part, &mut table) else {
            continue;
        };
        let mut grid: Vec<[i64; 3]> = Vec::new();
        let mut edges = Vec::new();
        for f in &solid.faces {
            let at: Vec<usize> = f
                .polygon
                .corners
                .iter()
                .map(|c| {
                    let v = c.to_f64().round();
                    let g = [v.x as i64, v.y as i64, v.z as i64];
                    grid.iter().position(|&q| q == g).unwrap_or_else(|| {
                        grid.push(g);
                        grid.len() - 1
                    })
                })
                .collect();
            for k in 0..at.len() {
                let (a, b) = (at[k], at[(k + 1) % at.len()]);
                let edge = (a.min(b), a.max(b));
                if a != b && !edges.contains(&edge) {
                    edges.push(edge);
                }
            }
        }
        let world_points = grid
            .iter()
            .map(|g| {
                let v = DVec3::new(g[0] as f64, g[1] as f64, g[2] as f64) * UNIT;
                world.transform_point3(v.as_vec3())
            })
            .collect();
        out.push(PartCorners {
            part: p,
            grid,
            world: world_points,
            edges,
        });
    }
}

/// Corners or an edge being dragged: moved together in the plane of the brush's own axes
/// that faces the camera most, the part made again as the hull of its corners.
#[derive(Debug, Clone)]
pub(crate) struct CornerDrag {
    pub(crate) part: usize,
    /// The part's corners when the drag began, and which of them move.
    corners: Vec<[i64; 3]>,
    moving: Vec<usize>,
    /// The brush's planes then.
    start: Vec<Vec<Plane>>,
    /// Where the pointer took them, the plane they move in, and the brush's placement.
    grab: Vec3,
    normal: Vec3,
    world: Affine3A,
    /// Its undo step; taken back (Escape).
    pub(crate) merge: u64,
    pub(crate) cancelled: bool,
    /// How far they have gone in the brush's own space, metres, for the label.
    pub(crate) moved: Vec3,
}

impl CornerDrag {
    /// A drag of corners `moving` of part `points` of `brush` placed at `world`, taken at
    /// `grab`, seen from `eye`, as undo step `merge`.
    pub(crate) fn start(
        brush: &Brush,
        world: Affine3A,
        points: &PartCorners,
        moving: Vec<usize>,
        grab: Vec3,
        eye: Vec3,
        merge: u64,
    ) -> Self {
        let toward = (eye - grab).normalize_or_zero();
        let axes =
            [Vec3::X, Vec3::Y, Vec3::Z].map(|a| world.transform_vector3(a).normalize_or_zero());
        let normal = axes
            .into_iter()
            .max_by(|a, b| a.dot(toward).abs().total_cmp(&b.dot(toward).abs()))
            .unwrap_or(Vec3::Y);
        Self {
            part: points.part,
            corners: points.grid.clone(),
            moving,
            start: brush.parts(),
            grab,
            normal,
            world,
            merge,
            cancelled: false,
            moved: Vec3::ZERO,
        }
    }

    /// The brush's planes with the corners moved where the pointer at `at` takes them, on
    /// the grid of the position step along each of the brush's own axes as the rules say
    /// (free: the millimetres), as its `planes`; none when they make no solid (the drag
    /// keeps its last).
    pub(crate) fn planes(
        &mut self,
        view: &ViewProj,
        at: Vec2,
        snap: &Snapping,
        invert: bool,
        round: bool,
    ) -> Option<Vec<BrushPlane>> {
        let hit = hit_plane(view.ray(at), self.grab, self.normal)?;
        let own = self.world.inverse().transform_vector3(hit - self.grab);
        let step = |v: f32| snap.snap(Quantity::Position, 0.0, f64::from(v), invert, round);
        let by = [0, 1, 2].map(|i| (step(own[i]) / UNIT).round() as i64);
        let mut corners = self.corners.clone();
        for &k in &self.moving {
            for i in 0..3 {
                corners[k][i] += by[i];
            }
        }
        let planes = engine_brush::shape::hull(&corners)?;
        let mut parts = self.start.clone();
        parts[self.part] = planes;
        if !whole(&parts) {
            return None;
        }
        self.moved = Vec3::new(by[0] as f32, by[1] as f32, by[2] as f32) * UNIT as f32;
        Some(as_planes(&parts))
    }
}

/// The planes of brush parts as a brush's `planes` field.
fn as_planes(parts: &[Vec<Plane>]) -> Vec<BrushPlane> {
    parts
        .iter()
        .enumerate()
        .flat_map(|(k, part)| {
            part.iter().map(move |q| BrushPlane {
                part: k as u32,
                normal: q.n,
                offset: q.d,
            })
        })
        .collect()
}

/// Whether every part of `parts` makes a closed solid.
fn whole(parts: &[Vec<Plane>]) -> bool {
    let mut table = Planes::new();
    parts.iter().all(|part| solid(part, &mut table).is_some())
}

/// A face being dragged: moved along its normal, or pulled into a new brush (`extrude`).
#[derive(Debug, Clone)]
pub(crate) struct FaceDrag {
    /// The face: its part and plane.
    pub(crate) part: usize,
    pub(crate) plane: usize,
    /// The brush's planes when the drag began (a room's inside's), in its own grid; and
    /// the face as it shows (a room's wall's outside) with its polygon there, where a pull
    /// starts.
    start: Vec<Vec<Plane>>,
    shown: Plane,
    polygon: Vec<DVec3>,
    /// Where the pointer took the face, and its outward direction, in the world.
    grab: Vec3,
    dir: Vec3,
    /// World metres per millimetre of the brush's own along the face's normal.
    unit: f32,
    /// Pull a new brush out instead of moving the face.
    pub(crate) extrude: bool,
    /// Its undo step; taken back (Escape).
    pub(crate) merge: u64,
    pub(crate) cancelled: bool,
    /// How far it has gone, metres (outward positive), for its label.
    pub(crate) distance: f32,
}

impl FaceDrag {
    /// A drag of face `face` of `brush` placed at `world`, taken at `grab`, as undo step
    /// `merge`; `extrude` pulls a new brush out of it. None when the face's planes make no
    /// solid.
    pub(crate) fn start(
        brush: &Brush,
        world: Affine3A,
        face: &Face,
        grab: Vec3,
        extrude: bool,
        merge: u64,
    ) -> Option<Self> {
        let start = base_parts(brush);
        let plane = *start.get(face.part)?.get(face.plane)?;
        let outside = brush.parts();
        let shown = *outside.get(face.part)?.get(face.plane)?;
        let mut table = Planes::new();
        let solid = solid(&outside[face.part], &mut table)?;
        let polygon = solid
            .faces
            .iter()
            .find(|f| f.owner.face as usize == face.plane)?
            .polygon
            .corners
            .iter()
            .map(|c| c.to_f64())
            .collect();
        let n = Vec3::new(plane.n[0] as f32, plane.n[1] as f32, plane.n[2] as f32).normalize();
        let unit = world.transform_vector3(n * UNIT as f32).length().max(1e-9);
        Some(Self {
            part: face.part,
            plane: face.plane,
            start,
            shown,
            polygon,
            grab,
            dir: face.normal,
            unit,
            extrude,
            merge,
            cancelled: false,
            distance: 0.0,
        })
    }

    /// The distance the pointer at `at` takes the face (metres, outward positive): along
    /// its normal, on the plane holding that line that faces the camera most, snapped by
    /// the rules with Shift (`invert`) and Ctrl (`round`), on the millimetres.
    pub(crate) fn travel(
        &self,
        view: &ViewProj,
        at: Vec2,
        snap: &Snapping,
        invert: bool,
        round: bool,
    ) -> Option<f32> {
        let toward = (view.eye - self.grab).normalize_or_zero();
        let across = self.dir.cross(toward.cross(self.dir)).try_normalize()?;
        let free = (hit_plane(view.ray(at), self.grab, across)? - self.grab).dot(self.dir);
        let value = snap.snap(Quantity::Position, 0.0, f64::from(free), invert, round) as f32;
        Some((value * 1000.0).round() / 1000.0)
    }

    /// The brush's planes with the face moved `distance` metres out (in, negative), as its
    /// `planes`; none when a part would be left empty (the drag keeps its last).
    pub(crate) fn moved(&mut self, distance: f32) -> Option<Vec<BrushPlane>> {
        let mut parts = self.start.clone();
        let by = f64::from(distance / self.unit);
        parts[self.part][self.plane] = parts[self.part][self.plane].offset(by)?;
        if !whole(&parts) {
            return None;
        }
        self.distance = distance;
        Some(as_planes(&parts))
    }

    /// The brush a pull makes at `distance` metres: out of the face, an adding brush between
    /// the face and its plane moved out; pushed in, a cut of the face's outline that deep,
    /// a millimetre proud of the face so it opens it. Its planes are in the dragged brush's
    /// own grid (it takes the same place). None for no distance.
    pub(crate) fn pulled(&mut self, distance: f32) -> Option<(BrushOp, Vec<BrushPlane>)> {
        if distance.abs() < 0.001 {
            return None;
        }
        let face = self.shown;
        let n = DVec3::new(
            f64::from(face.n[0]),
            f64::from(face.n[1]),
            f64::from(face.n[2]),
        );
        let by = f64::from(distance / self.unit);
        let (op, near, far) = if by > 0.0 {
            (BrushOp::Add, face.flip(), face.offset(by)?)
        } else {
            (BrushOp::Cut, face.offset(1.0)?, face.offset(by)?.flip())
        };
        let mut planes = vec![near, far];
        let unit_n = n.normalize();
        for k in 0..self.polygon.len() {
            let (a, b) = (self.polygon[k], self.polygon[(k + 1) % self.polygon.len()]);
            // Out of the polygon, across its edge (counter-clockwise seen from in front).
            planes.push(Plane::from_f64((b - a).cross(unit_n), a)?);
        }
        if !whole(std::slice::from_ref(&planes)) {
            return None;
        }
        self.distance = distance;
        Some((op, as_planes(&[planes])))
    }
}

/// `brush` with `planes` as its own: edited, its shape's parameters gone (a room keeps
/// its walls around them).
pub(crate) fn edited(brush: &Brush, planes: Vec<BrushPlane>) -> Brush {
    Brush {
        shape: BrushShape::Planes,
        planes,
        ..brush.clone()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::camera::OrbitCamera;

    fn view() -> ViewProj {
        let camera = OrbitCamera {
            target: Vec3::new(0.0, 1.0, 0.0),
            yaw: 0.5,
            pitch: 0.5,
            distance: 12.0,
            ..OrbitCamera::default()
        };
        ViewProj::new(&camera, [0, 0, 1200, 900])
    }

    /// The volume of `planes`' parts, cubic metres.
    fn volume(planes: &[BrushPlane]) -> f64 {
        let brush = edited(&Brush::default(), planes.to_vec());
        let mut out = Vec::new();
        faces(&brush, Affine3A::IDENTITY, &mut out);
        out.iter()
            .map(|f| {
                let c = &f.corners;
                (1..c.len() - 1)
                    .map(|i| f64::from(c[0].dot(c[i].cross(c[i + 1]))) / 6.0)
                    .sum::<f64>()
            })
            .sum()
    }

    #[test]
    fn a_boxs_faces_are_six_and_a_ray_takes_the_one_it_meets() {
        let brush = Brush::new(BrushShape::Box, Vec3::new(2.0, 1.0, 2.0));
        let mut out = Vec::new();
        faces(
            &brush,
            Affine3A::from_translation(Vec3::new(0.0, 0.0, 5.0)),
            &mut out,
        );
        assert_eq!(out.len(), 6);
        // From above: the top, at y = 1.
        let (i, t) = pick(&out, (Vec3::new(0.3, 10.0, 5.2), Vec3::NEG_Y)).unwrap();
        assert!(out[i].normal.distance(Vec3::Y) < 1e-6 && (t - 9.0).abs() < 1e-5);
        // From inside: the face it leaves through.
        let (i, _) = pick(&out, (Vec3::new(0.0, 0.5, 5.0), Vec3::X)).unwrap();
        assert!(out[i].normal.distance(Vec3::X) < 1e-6);
        assert!(pick(&out, (Vec3::new(5.0, 10.0, 5.0), Vec3::NEG_Y)).is_none());
    }

    #[test]
    fn a_face_moves_along_its_normal_exactly_and_stops_before_nothing_is_left() {
        let brush = Brush::new(BrushShape::Box, Vec3::splat(2.0));
        let mut out = Vec::new();
        faces(&brush, Affine3A::IDENTITY, &mut out);
        let top = out.iter().find(|f| f.normal.y > 0.9).unwrap().clone();
        let mut d =
            FaceDrag::start(&brush, Affine3A::IDENTITY, &top, top.middle(), false, 1).unwrap();
        let planes = d.moved(0.5).unwrap();
        assert!(
            (volume(&planes) - 2.0 * 2.5 * 2.0).abs() < 1e-6,
            "{}",
            volume(&planes)
        );
        assert_eq!(d.distance, 0.5);
        // Down through the base: refused, the last kept.
        assert!(d.moved(-2.5).is_none());
        assert_eq!(d.distance, 0.5);
        // The pointer 0.43 m up the top's normal, with 0.1 m steps: 0.4.
        let view = view();
        let snap = Snapping {
            on: true,
            ..Snapping::default()
        };
        let at = view.screen(top.middle() + Vec3::Y * 0.43).unwrap();
        assert!((d.travel(&view, at, &snap, false, false).unwrap() - 0.4).abs() < 1e-6);
    }

    #[test]
    fn a_rooms_face_moves_its_insides_plane_and_its_wall_follows() {
        let room = Brush::new(BrushShape::Box, Vec3::new(4.0, 3.0, 4.0)).with_hollow(0.2);
        let mut out = Vec::new();
        faces(&room, Affine3A::IDENTITY, &mut out);
        // What shows is the walls' outside: +x at 2.2 m.
        let east = out.iter().find(|f| f.normal.x > 0.9).unwrap().clone();
        assert!((east.corners[0].x - 2.2).abs() < 1e-5);
        let mut d =
            FaceDrag::start(&room, Affine3A::IDENTITY, &east, east.middle(), false, 1).unwrap();
        let moved = edited(&room, d.moved(1.0).unwrap());
        assert!(moved.is_room());
        // Its inside is 1 m wider, and the wall stands around it again.
        let mut inside = Vec::new();
        faces(
            &edited(&Brush::default(), as_planes(&moved.inside())),
            Affine3A::IDENTITY,
            &mut inside,
        );
        let x = inside.iter().find(|f| f.normal.x > 0.9).unwrap().corners[0].x;
        assert!((x - 3.0).abs() < 1e-5, "{x}");
        faces(&moved, Affine3A::IDENTITY, &mut out);
        let x = out.iter().find(|f| f.normal.x > 0.9).unwrap().corners[0].x;
        assert!((x - 3.2).abs() < 1e-5, "{x}");
    }

    #[test]
    fn a_boxs_corners_are_eight_and_its_edges_twelve_a_rooms_none() {
        let brush = Brush::new(BrushShape::Box, Vec3::new(2.0, 1.0, 2.0));
        let mut out = Vec::new();
        corners(&brush, Affine3A::IDENTITY, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].grid.len(), out[0].edges.len()), (8, 12));
        assert!(out[0].grid.contains(&[1000, 1000, -1000]));
        let room = brush.clone().with_hollow(0.2);
        corners(&room, Affine3A::IDENTITY, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn an_edge_dragged_down_onto_another_makes_a_wedge() {
        let view = view();
        let snap = Snapping {
            on: true,
            ..Snapping::default()
        };
        let brush = Brush::new(BrushShape::Box, Vec3::new(2.0, 1.0, 2.0));
        let mut out = Vec::new();
        corners(&brush, Affine3A::IDENTITY, &mut out);
        let points = &out[0];
        // The top's edge at z = +1 m, its two corners.
        let moving: Vec<usize> = (0..points.grid.len())
            .filter(|&k| points.grid[k][1] == 1000 && points.grid[k][2] == 1000)
            .collect();
        assert_eq!(moving.len(), 2);
        let grab = Vec3::new(0.0, 1.0, 1.0);
        let mut d = CornerDrag::start(
            &brush,
            Affine3A::IDENTITY,
            points,
            moving,
            grab,
            view.eye,
            1,
        );
        // Seen from the front and above: it moves in the plane across z (x and y), down 1 m.
        let at = view.screen(grab - Vec3::Y).unwrap();
        let planes = d.planes(&view, at, &snap, false, false).unwrap();
        let wedge = edited(&brush, planes);
        let mut faces_now = Vec::new();
        faces(&wedge, Affine3A::IDENTITY, &mut faces_now);
        assert_eq!(faces_now.len(), 5, "a wedge");
        assert!((volume(&wedge.planes) - 2.0 * 1.0 * 2.0 / 2.0).abs() < 1e-5);
        assert!(
            (d.moved - Vec3::new(0.0, -1.0, 0.0)).length() < 1e-6,
            "{}",
            d.moved
        );
    }

    #[test]
    fn a_pull_out_of_a_room_starts_at_its_walls_outside() {
        let room = Brush::new(BrushShape::Box, Vec3::new(4.0, 3.0, 4.0)).with_hollow(0.2);
        let mut out = Vec::new();
        faces(&room, Affine3A::IDENTITY, &mut out);
        let east = out.iter().find(|f| f.normal.x > 0.9).unwrap().clone();
        let mut d =
            FaceDrag::start(&room, Affine3A::IDENTITY, &east, east.middle(), true, 1).unwrap();
        let (_, planes) = d.pulled(1.0).unwrap();
        let mut pulled = Vec::new();
        faces(
            &edited(&Brush::default(), planes),
            Affine3A::IDENTITY,
            &mut pulled,
        );
        let xs: Vec<f32> = pulled
            .iter()
            .flat_map(|f| f.corners.iter().map(|c| c.x))
            .collect();
        let lo = xs.iter().copied().fold(f32::MAX, f32::min);
        assert!((lo - 2.2).abs() < 1e-5, "{lo}");
    }

    #[test]
    fn a_pull_out_adds_a_brush_on_the_face_and_a_push_in_cuts_its_outline() {
        let brush = Brush::new(BrushShape::Box, Vec3::new(2.0, 1.0, 2.0));
        let mut out = Vec::new();
        faces(&brush, Affine3A::IDENTITY, &mut out);
        let top = out.iter().find(|f| f.normal.y > 0.9).unwrap().clone();
        let mut d =
            FaceDrag::start(&brush, Affine3A::IDENTITY, &top, top.middle(), true, 1).unwrap();
        let (op, planes) = d.pulled(0.5).unwrap();
        assert_eq!(op, BrushOp::Add);
        assert!(
            (volume(&planes) - 2.0 * 0.5 * 2.0).abs() < 1e-6,
            "{}",
            volume(&planes)
        );
        // Its base on the top: from 1 m to 1.5 m.
        let mut pulled = Vec::new();
        faces(
            &edited(&Brush::default(), planes),
            Affine3A::IDENTITY,
            &mut pulled,
        );
        let ys: Vec<f32> = pulled
            .iter()
            .flat_map(|f| f.corners.iter().map(|c| c.y))
            .collect();
        let (lo, hi) = ys
            .iter()
            .fold((f32::MAX, f32::MIN), |(l, h), &y| (l.min(y), h.max(y)));
        assert!(
            (lo - 1.0).abs() < 1e-5 && (hi - 1.5).abs() < 1e-5,
            "{lo} {hi}"
        );
        // In: a cut 0.3 m deep, a millimetre proud of the top.
        let (op, planes) = d.pulled(-0.3).unwrap();
        assert_eq!(op, BrushOp::Cut);
        assert!(
            (volume(&planes) - 2.0 * 0.301 * 2.0).abs() < 1e-6,
            "{}",
            volume(&planes)
        );
        assert!(d.pulled(0.0).is_none());
    }
}
