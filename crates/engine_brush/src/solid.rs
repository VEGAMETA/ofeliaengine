//! Convex solids: the faces of the planes that bound them ([`Solid`]), split by planes
//! exactly, and the CSG of two of them: one cut out of another (convex pieces), one
//! clipped to another.

use crate::exact::Point;
use crate::polygon::{Clipped, PlaneId, Planes, Polygon, Split};
use core::cmp::Ordering;

/// Which brush face a face came from: the brush's place in its group and the face's
/// place in the brush.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner {
    /// The brush.
    pub brush: u32,
    /// Its face.
    pub face: u32,
}

/// A face of a solid: its polygon and the brush face it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Face {
    /// What made it.
    pub owner: Owner,
    /// Its polygon (on the face's plane).
    pub polygon: Polygon,
}

/// A convex solid of positive volume: the faces of the planes that bound it with some
/// area, at least four.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Solid {
    /// Its faces.
    pub faces: Vec<Face>,
    /// Its bounds in grid units, rounded outwards: lowest corner.
    pub lo: [i64; 3],
    /// And highest.
    pub hi: [i64; 3],
}

/// Why planes make no solid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolidError {
    /// Nothing lies behind all of them.
    Empty,
    /// What lies behind them is not closed (or reaches beyond [`crate::exact::MAX_COORD`]).
    Open,
}

impl Solid {
    /// What lies behind all of `planes`, each face owned as given (a plane given twice
    /// keeps its first owner).
    pub fn from_planes(planes: &[(PlaneId, Owner)], table: &Planes) -> Result<Self, SolidError> {
        let mut sides = Vec::new();
        let mut faces = Vec::with_capacity(planes.len());
        for (k, &(f, owner)) in planes.iter().enumerate() {
            if planes[..k].iter().any(|&(g, _)| g == f) {
                continue;
            }
            if planes.iter().any(|&(g, _)| g == f.flip()) {
                return Err(SolidError::Empty);
            }
            let Some(mut polygon) = Polygon::square(f, table) else {
                return Err(SolidError::Empty);
            };
            let mut gone = false;
            for &(g, _) in planes {
                if g == f {
                    continue;
                }
                match polygon.clip(g, table, &mut sides) {
                    Clipped::Whole => {}
                    Clipped::Gone => {
                        gone = true;
                        break;
                    }
                    Clipped::Part(p) => polygon = p,
                }
            }
            if gone {
                continue;
            }
            if polygon.touches_bound() {
                return Err(SolidError::Open);
            }
            faces.push(Face { owner, polygon });
        }
        if faces.len() < 4 {
            return Err(if faces.is_empty() {
                SolidError::Empty
            } else {
                SolidError::Open
            });
        }
        Ok(Self::with_faces(faces))
    }

    fn with_faces(faces: Vec<Face>) -> Self {
        let (mut lo, mut hi) = ([i64::MAX; 3], [i64::MIN; 3]);
        for f in &faces {
            for c in &f.polygon.corners {
                let p = c.to_f64();
                for (i, v) in [p.x, p.y, p.z].into_iter().enumerate() {
                    lo[i] = lo[i].min(v.floor() as i64 - 1);
                    hi[i] = hi[i].max(v.ceil() as i64 + 1);
                }
            }
        }
        Self { faces, lo, hi }
    }

    /// Whether its bounds and `other`'s touch or overlap.
    #[inline]
    pub fn near(&self, other: &Self) -> bool {
        (0..3).all(|i| self.lo[i] <= other.hi[i] && other.lo[i] <= self.hi[i])
    }

    /// The corners of its faces (each corner once per face it is on).
    pub fn corners(&self) -> impl Iterator<Item = &Point> {
        self.faces.iter().flat_map(|f| f.polygon.corners.iter())
    }

    /// The part of it behind `s` and the part in front, the new face on `s` owned by
    /// `owner` (turned around on the part in front).
    pub fn split(
        &self,
        s: PlaneId,
        owner: Owner,
        table: &Planes,
        sides: &mut Vec<Ordering>,
    ) -> Split<Self> {
        let plane = table.get(s);
        let (mut behind, mut front) = (false, false);
        for c in self.corners() {
            match c.side(plane) {
                Ordering::Less => behind = true,
                Ordering::Greater => front = true,
                Ordering::Equal => {}
            }
            if behind && front {
                break;
            }
        }
        match (behind, front) {
            (_, false) => return Split::Behind,
            (false, true) => return Split::Front,
            (true, true) => {}
        }
        let mut back_faces = Vec::with_capacity(self.faces.len() + 1);
        let mut front_faces = Vec::with_capacity(self.faces.len() + 1);
        for f in &self.faces {
            match f.polygon.split(s, table, sides) {
                Split::Behind => back_faces.push(f.clone()),
                Split::Front => front_faces.push(f.clone()),
                Split::Both(b, fr) => {
                    back_faces.push(Face {
                        owner: f.owner,
                        polygon: b,
                    });
                    front_faces.push(Face {
                        owner: f.owner,
                        polygon: fr,
                    });
                }
            }
        }
        // The new face: the plane's square clipped by every face's plane.
        let Some(mut cap) = Polygon::square(s, table) else {
            return Split::Behind;
        };
        for f in &self.faces {
            match cap.clip(f.polygon.on, table, sides) {
                Clipped::Whole => {}
                Clipped::Gone => return Split::Behind,
                Clipped::Part(p) => cap = p,
            }
        }
        front_faces.push(Face {
            owner,
            polygon: cap.reversed(),
        });
        back_faces.push(Face {
            owner,
            polygon: cap,
        });
        Split::Both(Self::with_faces(back_faces), Self::with_faces(front_faces))
    }

    /// It with `cut` taken out: the convex pieces left, each face where `cut` was owned by
    /// `cut`'s face. `None` when they do not overlap (it is left whole).
    pub fn subtract(
        &self,
        cut: &Self,
        table: &Planes,
        sides: &mut Vec<Ordering>,
    ) -> Option<Vec<Self>> {
        if !self.near(cut) {
            return None;
        }
        let mut out = Vec::new();
        let mut rest: Option<Self> = None;
        for face in cut.faces_axes_first(table) {
            let current = rest.as_ref().unwrap_or(self);
            match current.split(face.polygon.on, face.owner, table, sides) {
                Split::Behind => {}
                Split::Front => return None,
                Split::Both(b, f) => {
                    out.push(f);
                    rest = Some(b);
                }
            }
        }
        // What is left lies inside the cut.
        Some(out)
    }

    /// The part of it inside `other`, the new faces owned by `other`'s faces; `None` when
    /// none is.
    pub fn intersect(
        &self,
        other: &Self,
        table: &Planes,
        sides: &mut Vec<Ordering>,
    ) -> Option<Self> {
        if !self.near(other) {
            return None;
        }
        let mut rest: Option<Self> = None;
        for face in &other.faces {
            let current = rest.as_ref().unwrap_or(self);
            match current.split(face.polygon.on, face.owner, table, sides) {
                Split::Behind => {}
                Split::Front => return None,
                Split::Both(b, _) => rest = Some(b),
            }
        }
        Some(rest.unwrap_or_else(|| self.clone()))
    }

    /// Its faces, those facing along an axis first (TrenchBroom's order for cuts: the
    /// pieces come out as slabs).
    fn faces_axes_first<'a>(&'a self, table: &Planes) -> Vec<&'a Face> {
        let mut order: Vec<&Face> = self.faces.iter().collect();
        order.sort_by_key(|f| {
            let n = table.get(f.polygon.on).n;
            u8::from(n.iter().filter(|&&c| c == 0).count() < 2)
        });
        order
    }
}
