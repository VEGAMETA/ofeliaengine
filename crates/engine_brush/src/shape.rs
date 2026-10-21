//! The planes of the shapes brushes start as, in grid units.

use crate::exact::Plane;
use crate::polygon::{PlaneId, Planes};

/// The six planes of the box from `lo` to `hi` (+x, -x, +y, -y, +z, -z); `None` when a
/// side is out of bounds.
pub fn box_planes(lo: [i64; 3], hi: [i64; 3], table: &mut Planes) -> Option<Vec<PlaneId>> {
    let mut out = Vec::with_capacity(6);
    for axis in 0..3 {
        let mut n = [0; 3];
        n[axis] = 1;
        out.push(table.add(Plane::new(n, hi[axis])?));
        n[axis] = -1;
        out.push(table.add(Plane::new(n, -lo[axis])?));
    }
    Some(out)
}
