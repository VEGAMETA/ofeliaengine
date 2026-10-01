//! Skeletons: joints in depth-first order (every parent before its children, every
//! subtree contiguous), parents as indices, a rest pose.

use crate::FormatError;
use crate::bytes::{Reader, Writer};
use crate::pose::{Pose, Transform};
use engine_core::math::{Quat, Vec3, Vec3A};

/// Marks a root joint in [`Skeleton::parents`].
pub const NO_PARENT: u16 = u16::MAX;

/// Most joints a skeleton can have.
pub const MAX_JOINTS: usize = u16::MAX as usize;

/// A joint as given to [`Skeleton::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct JointDesc {
    /// Name (for lookups and tools).
    pub name: String,
    /// Index of the parent among the joints given, or `None` for a root.
    pub parent: Option<usize>,
    /// Transform relative to the parent in the rest (bind) pose.
    pub rest: Transform,
}

/// Why a skeleton could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkeletonError {
    /// More than [`MAX_JOINTS`] joints.
    TooManyJoints,
    /// A parent index past the joints.
    BadParent(usize),
    /// The parents form a cycle (the joint is on it).
    Cycle(usize),
}

impl core::fmt::Display for SkeletonError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyJoints => write!(f, "more than {MAX_JOINTS} joints"),
            Self::BadParent(j) => write!(f, "joint {j} has a parent past the joints"),
            Self::Cycle(j) => write!(f, "joint {j} is its own ancestor"),
        }
    }
}

impl std::error::Error for SkeletonError {}

/// A skeleton (see the module documentation).
///
/// Joints are in depth-first preorder: `parents[j] < j`, and the descendants of `j` are
/// exactly `j + 1 .. subtree_end(j)`. Local-to-model conversion is then one forward loop,
/// and a mask of a subtree (an arm, the upper body) is a range.
#[derive(Debug, Clone, PartialEq)]
pub struct Skeleton {
    parents: Vec<u16>,
    names: Vec<String>,
    subtree_ends: Vec<u16>,
    rest: Pose,
}

impl Skeleton {
    /// A skeleton from joints in any order. Returns it with the new index of each joint
    /// given (`remap[old] = new`). Children keep their relative order.
    pub fn new(joints: &[JointDesc]) -> Result<(Self, Vec<usize>), SkeletonError> {
        let n = joints.len();
        if n > MAX_JOINTS {
            return Err(SkeletonError::TooManyJoints);
        }
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut roots = Vec::new();
        for (j, joint) in joints.iter().enumerate() {
            match joint.parent {
                Some(p) if p >= n => return Err(SkeletonError::BadParent(j)),
                Some(p) => children[p].push(j),
                None => roots.push(j),
            }
        }
        let mut order = Vec::with_capacity(n);
        let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
        while let Some(j) = stack.pop() {
            order.push(j);
            stack.extend(children[j].iter().rev());
        }
        if order.len() != n {
            // Joints never reached from a root sit on a cycle.
            let mut seen = vec![false; n];
            for &j in &order {
                seen[j] = true;
            }
            let j = seen.iter().position(|s| !s).unwrap_or(0);
            return Err(SkeletonError::Cycle(j));
        }
        let mut remap = vec![0; n];
        for (new, &old) in order.iter().enumerate() {
            remap[old] = new;
        }
        let parents: Vec<u16> = order
            .iter()
            .map(|&old| joints[old].parent.map_or(NO_PARENT, |p| remap[p] as u16))
            .collect();
        let rest: Vec<Transform> = order.iter().map(|&old| joints[old].rest).collect();
        let names = order.iter().map(|&old| joints[old].name.clone()).collect();
        Ok((
            Self::from_parts(parents, names, Pose::from_transforms(&rest)),
            remap,
        ))
    }

    fn from_parts(parents: Vec<u16>, names: Vec<String>, rest: Pose) -> Self {
        let n = parents.len();
        let mut subtree_ends = vec![0u16; n];
        // In preorder, a joint's subtree ends where the next joint that is not its
        // descendant starts: walk backwards, extending each parent by its child's end.
        for j in (0..n).rev() {
            let end = subtree_ends[j].max(j as u16 + 1);
            subtree_ends[j] = end;
            if let Some(&p) = parents.get(j)
                && p != NO_PARENT
            {
                let pe = &mut subtree_ends[p as usize];
                *pe = (*pe).max(end);
            }
        }
        Self {
            parents,
            names,
            subtree_ends,
            rest,
        }
    }

    /// Number of joints.
    #[inline]
    pub fn joint_count(&self) -> usize {
        self.parents.len()
    }

    /// Parent indices ([`NO_PARENT`] for roots); `parents()[j] < j`.
    #[inline]
    pub fn parents(&self) -> &[u16] {
        &self.parents
    }

    /// Joint `j`'s parent.
    #[inline]
    pub fn parent(&self, j: usize) -> Option<usize> {
        match self.parents.get(j) {
            Some(&p) if p != NO_PARENT => Some(p as usize),
            _ => None,
        }
    }

    /// Joint names.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The first joint called `name`.
    pub fn find(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// One past the last descendant of `j`: its subtree is `j .. subtree_end(j)`.
    #[inline]
    pub fn subtree_end(&self, j: usize) -> usize {
        self.subtree_ends.get(j).map_or(j, |&e| e as usize)
    }

    /// Whether `a` is `b` or one of its ancestors.
    pub fn is_ancestor_or_self(&self, a: usize, b: usize) -> bool {
        a <= b && b < self.subtree_end(a)
    }

    /// The rest (bind) pose.
    pub fn rest(&self) -> &Pose {
        &self.rest
    }

    /// The cooked bytes (see [`Self::from_bytes`]).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(SKELETON_MAGIC);
        w.u32(self.joint_count() as u32);
        for j in 0..self.joint_count() {
            w.u16(self.parents[j]);
            let t = self.rest.get(j);
            w.f32s(&t.translation.to_array());
            w.f32s(&t.rotation.to_array());
            w.f32s(&t.scale.to_array());
            w.str(&self.names[j]);
        }
        w.bytes
    }

    /// A skeleton from [`Self::to_bytes`], validated (parents before children).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut r = Reader { bytes };
        if r.u32()? != SKELETON_MAGIC {
            return Err(FormatError("not a skeleton"));
        }
        let n = r.u32()? as usize;
        if n > MAX_JOINTS || n * 42 > bytes.len() {
            return Err(FormatError("bad joint count"));
        }
        let mut parents = Vec::with_capacity(n);
        let mut names = Vec::with_capacity(n);
        let mut rest = Vec::with_capacity(n);
        for j in 0..n {
            let p = r.u16()?;
            if p != NO_PARENT && p as usize >= j {
                return Err(FormatError("parent after child"));
            }
            parents.push(p);
            let t = Vec3::from(r.f32s::<3>()?);
            let q = Quat::from_array(r.f32s::<4>()?);
            let s = Vec3A::from(r.f32s::<3>()?);
            rest.push(Transform {
                translation: t.into(),
                rotation: q,
                scale: s,
            });
            names.push(r.str()?);
        }
        r.finish()?;
        let skeleton = Self::from_parts(parents, names, Pose::from_transforms(&rest));
        // Preorder: a joint's parent is the joint before it or one of that joint's
        // ancestors (so every subtree is contiguous).
        for j in 1..n {
            if let Some(p) = skeleton.parent(j) {
                let mut a = Some(j - 1);
                while let Some(k) = a
                    && k != p
                {
                    a = skeleton.parent(k);
                }
                if a.is_none() {
                    return Err(FormatError("not in depth-first order"));
                }
            }
        }
        Ok(skeleton)
    }
}

const SKELETON_MAGIC: u32 = u32::from_le_bytes(*b"SKL1");

#[cfg(test)]
mod tests {
    use super::*;

    fn joint(name: &str, parent: Option<usize>) -> JointDesc {
        JointDesc {
            name: name.into(),
            parent,
            rest: Transform::new(Vec3::X, Quat::IDENTITY, Vec3::ONE),
        }
    }

    #[test]
    fn joints_are_sorted_depth_first() {
        // hand(0) -> arm(2) -> root(3); leg(1) -> root(3); finger(4) -> hand(0).
        let joints = [
            joint("hand", Some(2)),
            joint("leg", Some(3)),
            joint("arm", Some(3)),
            joint("root", None),
            joint("finger", Some(0)),
        ];
        let (s, remap) = Skeleton::new(&joints).unwrap();
        let names: Vec<&str> = s.names().iter().map(String::as_str).collect();
        assert_eq!(names, ["root", "leg", "arm", "hand", "finger"]);
        assert_eq!(remap, [3, 1, 2, 0, 4]);
        for j in 0..5 {
            if let Some(p) = s.parent(j) {
                assert!(p < j);
            }
        }
        assert_eq!(s.subtree_end(0), 5);
        assert_eq!(s.subtree_end(1), 2);
        assert_eq!(s.subtree_end(2), 5);
        assert!(s.is_ancestor_or_self(2, 4));
        assert!(!s.is_ancestor_or_self(1, 4));
        let back = Skeleton::from_bytes(&s.to_bytes()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn cycles_and_bad_parents_are_refused() {
        let cycle = [joint("a", Some(1)), joint("b", Some(0)), joint("c", None)];
        assert!(matches!(
            Skeleton::new(&cycle),
            Err(SkeletonError::Cycle(_))
        ));
        assert_eq!(
            Skeleton::new(&[joint("a", Some(5))]),
            Err(SkeletonError::BadParent(0))
        );
        assert!(Skeleton::from_bytes(b"SKL1\x01\x00").is_err());
    }
}
