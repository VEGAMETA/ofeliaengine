//! Assets in scene files (ADR 0108): our own files carry their [`AssetId`] in their header,
//! foreign ones in a `.meta` beside them, and a reference ([`AssetRef`]) is written with
//! its path first and the file's id after it:
//!
//! ```text
//! MeshRenderer.mesh = asset("props/crate.gltf#Crate", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)
//! ```

use engine_reflect::Reflect;
use std::collections::HashMap;
use std::fmt::Write as _;

/// The stable identity of an asset file (random 128 bits). Zero is no asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Reflect)]
#[repr(C)]
pub struct AssetId {
    hi: u64,
    lo: u64,
}

impl AssetId {
    /// No asset.
    pub const NONE: Self = Self { hi: 0, lo: 0 };

    /// A new random id, never zero.
    pub fn new() -> Self {
        loop {
            let id = Self::from_u128(crate::random_u128());
            if !id.is_none() {
                return id;
            }
        }
    }

    /// An id from its 128-bit value.
    pub const fn from_u128(v: u128) -> Self {
        Self {
            hi: (v >> 64) as u64,
            lo: v as u64,
        }
    }

    /// The 128-bit value.
    pub const fn as_u128(self) -> u128 {
        ((self.hi as u128) << 64) | self.lo as u128
    }

    /// Whether this is no asset.
    pub const fn is_none(self) -> bool {
        self.hi == 0 && self.lo == 0
    }

    /// An id from its 32 hex digits.
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() != 32 {
            return None;
        }
        u128::from_str_radix(s, 16).ok().map(Self::from_u128)
    }
}

impl core::fmt::Display for AssetId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:032x}", self.as_u128())
    }
}

/// A reference to an asset: a file, or a part inside one (a glTF's mesh or material).
///
/// Plain data, so components holding it stay copyable and undoable. The part is kept as
/// the hash of its name; the name and the file's path are written beside the id
/// ([`AssetPaths`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Reflect)]
#[repr(C)]
pub struct AssetRef {
    /// The file.
    pub file: AssetId,
    /// The part inside the file: 0 for the whole file, else [`AssetRef::part_hash`] of its
    /// name.
    pub part: u64,
}

impl AssetRef {
    /// No asset.
    pub const NONE: Self = Self {
        file: AssetId::NONE,
        part: 0,
    };

    /// A whole file.
    pub const fn file(file: AssetId) -> Self {
        Self { file, part: 0 }
    }

    /// A part of a file by its name (`"Crate"`, or `"#3"` by index as the colliders' rules
    /// name meshes).
    pub const fn part(file: AssetId, name: &str) -> Self {
        Self {
            file,
            part: Self::part_hash(name),
        }
    }

    /// The hash a part's name is kept as (FNV-1a, 64 bits), never 0.
    pub const fn part_hash(name: &str) -> u64 {
        let bytes = name.as_bytes();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut i = 0;
        while i < bytes.len() {
            h ^= bytes[i] as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
            i += 1;
        }
        if h == 0 { 1 } else { h }
    }

    /// Whether this is no asset.
    pub const fn is_none(self) -> bool {
        self.file.is_none()
    }

    /// The reference a written path names in the file `file`: `dir/file.ext` is the whole
    /// file, `dir/file.ext#Part` a part by name, `dir/file.ext#~<16 hex digits>` a part by
    /// its hash. The part starts at the first `#` of the file's name.
    ///
    /// # Errors
    ///
    /// When a `#~` hash is not 16 hex digits.
    pub fn from_path(file: AssetId, path: &str) -> Result<Self, String> {
        let name_start = path.rfind('/').map_or(0, |i| i + 1);
        let Some(hash) = path[name_start..].find('#') else {
            return Ok(Self::file(file));
        };
        let part = &path[name_start + hash + 1..];
        if let Some(hex) = part.strip_prefix('~') {
            let value = u64::from_str_radix(hex, 16)
                .ok()
                .filter(|&v| v != 0 && hex.len() == 16)
                .ok_or_else(|| format!("`#~{hex}` is not a part's 16 hex digits"))?;
            return Ok(Self { file, part: value });
        }
        Ok(Self::part(file, part))
    }

    /// Appends the reference as scene text: `asset("path#part", <id>)` with the path from
    /// `paths` when it names this reference, else the bare id with the part's hash;
    /// `none` for no asset.
    pub fn write(self, paths: Option<&AssetPaths>, out: &mut String) {
        if self.is_none() {
            out.push_str("none");
            return;
        }
        let known = paths
            .and_then(|p| p.get(self))
            .filter(|p| Self::from_path(self.file, p) == Ok(self));
        out.push_str("asset(");
        match known {
            Some(path) => crate::text::quote_into(out, path),
            None if self.part == 0 => out.push_str("\"\""),
            None => {
                let _ = write!(out, "\"#~{:016x}\"", self.part);
            }
        }
        let _ = write!(out, ", {})", self.file);
    }
}

/// The paths assets are written with, by reference (`props/crate.gltf#Crate`): filled by
/// loading a scene, or by the editor's asset database, and given to saving so references
/// keep readable paths.
#[derive(Debug, Clone, Default)]
pub struct AssetPaths {
    map: HashMap<AssetRef, String>,
}

impl AssetPaths {
    /// No paths.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the path a reference is written with (with `#part` for a part).
    pub fn insert(&mut self, asset: AssetRef, path: impl Into<String>) {
        self.map.insert(asset, path.into());
    }

    /// The path a reference is written with.
    pub fn get(&self, asset: AssetRef) -> Option<&str> {
        self.map.get(&asset).map(String::as_str)
    }

    /// Takes every path of `other`; its paths win.
    pub fn extend(&mut self, other: &AssetPaths) {
        for (&asset, path) in &other.map {
            self.map.insert(asset, path.clone());
        }
    }

    /// Every reference with its path, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (AssetRef, &str)> + '_ {
        self.map.iter().map(|(&asset, path)| (asset, path.as_str()))
    }

    /// How many references have a path.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether no reference has a path.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn paths_name_files_and_parts() {
        let id = AssetId::from_u128(7);
        assert_eq!(
            AssetRef::from_path(id, "props/crate.gltf"),
            Ok(AssetRef::file(id))
        );
        assert_eq!(
            AssetRef::from_path(id, "props/crate.gltf#Crate"),
            Ok(AssetRef::part(id, "Crate"))
        );
        // A `#` in a folder's name is part of the path.
        assert_eq!(
            AssetRef::from_path(id, "c#/crate.gltf#Lid"),
            Ok(AssetRef::part(id, "Lid"))
        );
        let lid = AssetRef::part(id, "Lid");
        assert_eq!(
            AssetRef::from_path(id, &format!("#~{:016x}", lid.part)),
            Ok(lid)
        );
        assert!(AssetRef::from_path(id, "x#~12").is_err());
        assert_ne!(AssetRef::part_hash(""), 0);
    }

    #[test]
    fn references_write_their_path_or_their_hash() {
        let id = AssetId::from_u128(0x5f2c);
        let lid = AssetRef::part(id, "Lid");
        let mut paths = AssetPaths::new();
        paths.insert(lid, "props/crate.gltf#Lid");
        let mut out = String::new();
        lid.write(Some(&paths), &mut out);
        assert_eq!(
            out,
            "asset(\"props/crate.gltf#Lid\", 00000000000000000000000000005f2c)"
        );
        out.clear();
        lid.write(None, &mut out);
        assert_eq!(
            AssetRef::from_path(id, &out[7..out.find("\", ").unwrap()]),
            Ok(lid),
            "{out}"
        );
        // A recorded path that names another part is not trusted.
        paths.insert(lid, "props/crate.gltf#Box");
        out.clear();
        lid.write(Some(&paths), &mut out);
        assert!(out.contains("#~"), "{out}");
        out.clear();
        AssetRef::NONE.write(Some(&paths), &mut out);
        assert_eq!(out, "none");
        assert_ne!(AssetId::new(), AssetId::new());
        assert_eq!(AssetId::parse(&id.to_string()), Some(id));
    }
}
