//! Material packs (ADR 0126's G2): a built game's material types compiled ahead, so that its
//! player makes their pipelines without Slang.
//!
//! A pack holds, for each material type the game's material files name (a hand-written
//! module's type, or a graph's generated module), the module's path under the game's folder
//! (`/` between folders), the type's name, its parameters' layout (Slang's reflection of
//! `<type>.Params`) and its fragment entry points' SPIR-V. The player registers a type it
//! finds there with both ([`MaterialPack::find`]); a type it does not find compiles as in the
//! editor.
//!
//! ```text
//! "OFMP"  u32 version  u128 key  u64 body bytes  zstd(body)
//! body:   u32 types, each: str module, str type, layout, u32 entries, each: str name,
//!         u32 words, words
//! layout: str name, u32 size, u32 fields, each: str name, u32 offset, u8 kind, u8 n,
//!         u32 count, u32 stride
//! str:    u32 bytes, UTF-8
//! ```
//!
//! The key is the build's hash of what the SPIR-V was compiled from (`engine_build`): a pack
//! whose key matches is not compiled again. It is read without the body
//! ([`MaterialPack::read_key`]).

use engine_shader::{Field, FieldKind, StructLayout};
use std::path::Path;

/// The pack's file name in a build's `data/` folder.
pub const MATERIAL_PACK: &str = "materials.pack";

const MAGIC: &[u8; 4] = b"OFMP";
const VERSION: u32 = 1;
/// Magic, version, key, body length.
const HEADER: usize = 4 + 4 + 16 + 8;
/// zstd's level for the body (SPIR-V compresses well; the build is not in a hurry).
const LEVEL: i32 = 19;

/// A material type compiled ahead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedType {
    /// Its module's path under the game's folder, `/` between folders.
    pub module: String,
    /// Its name in the module.
    pub type_name: String,
    /// Its parameters' layout.
    pub layout: StructLayout,
    /// Its fragment entry points' SPIR-V, by name.
    pub entries: Vec<(String, Vec<u32>)>,
}

/// A game's material types compiled ahead (see the module documentation).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterialPack {
    /// The hash of what it was compiled from.
    pub key: u128,
    /// Its types.
    pub types: Vec<PackedType>,
}

impl MaterialPack {
    /// The type `type_name` of the module at `module` (under the game's folder, `/` between
    /// folders).
    pub fn find(&self, module: &str, type_name: &str) -> Option<&PackedType> {
        self.types
            .iter()
            .find(|t| t.module == module && t.type_name == type_name)
    }

    /// The pack as its file's bytes.
    ///
    /// # Errors
    ///
    /// zstd failing.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        put_u32(&mut body, self.types.len());
        for t in &self.types {
            put_str(&mut body, &t.module);
            put_str(&mut body, &t.type_name);
            put_str(&mut body, &t.layout.name);
            body.extend_from_slice(&t.layout.size.to_le_bytes());
            put_u32(&mut body, t.layout.fields.len());
            for f in &t.layout.fields {
                put_str(&mut body, &f.name);
                body.extend_from_slice(&f.offset.to_le_bytes());
                let (kind, n) = match f.kind {
                    FieldKind::Float(n) => (0u8, n),
                    FieldKind::Uint(n) => (1, n),
                    FieldKind::Int(n) => (2, n),
                    FieldKind::Texture => (3, 1),
                };
                body.extend_from_slice(&[kind, n]);
                body.extend_from_slice(&f.count.to_le_bytes());
                body.extend_from_slice(&f.stride.to_le_bytes());
            }
            put_u32(&mut body, t.entries.len());
            for (name, words) in &t.entries {
                put_str(&mut body, name);
                put_u32(&mut body, words.len());
                for w in words {
                    body.extend_from_slice(&w.to_le_bytes());
                }
            }
        }
        let packed = zstd::bulk::compress(&body, LEVEL).map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(HEADER + packed.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.key.to_le_bytes());
        out.extend_from_slice(&(body.len() as u64).to_le_bytes());
        out.extend_from_slice(&packed);
        Ok(out)
    }

    /// A pack from its file's bytes.
    ///
    /// # Errors
    ///
    /// Not a material pack, another version, or a damaged one.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let key = key_of(bytes)?;
        let length = u64::from_le_bytes(bytes[24..HEADER].try_into().unwrap_or_default());
        let length = usize::try_from(length).map_err(|_| String::from("too large"))?;
        let body = zstd::bulk::decompress(&bytes[HEADER..], length).map_err(|e| e.to_string())?;
        let mut r = Reader { at: &body };
        let mut types = Vec::new();
        for _ in 0..r.u32()? {
            let module = r.str()?;
            let type_name = r.str()?;
            let name = r.str()?;
            let size = r.u32()?;
            let mut fields = Vec::new();
            for _ in 0..r.u32()? {
                let name = r.str()?;
                let offset = r.u32()?;
                let [kind, n] = r.take::<2>()?;
                let kind = match kind {
                    0 => FieldKind::Float(n),
                    1 => FieldKind::Uint(n),
                    2 => FieldKind::Int(n),
                    3 => FieldKind::Texture,
                    other => return Err(format!("a field of kind {other}")),
                };
                let count = r.u32()?;
                let stride = r.u32()?;
                fields.push(Field {
                    name,
                    offset,
                    kind,
                    count,
                    stride,
                });
            }
            let mut entries = Vec::new();
            for _ in 0..r.u32()? {
                let name = r.str()?;
                let words = r.u32()? as usize;
                let mut spirv = Vec::with_capacity(words);
                for _ in 0..words {
                    spirv.push(r.u32()?);
                }
                entries.push((name, spirv));
            }
            types.push(PackedType {
                module,
                type_name,
                layout: StructLayout { name, size, fields },
                entries,
            });
        }
        Ok(Self { key, types })
    }

    /// The pack at `path`; `None` when there is no file.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or is not a pack of this version.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_bytes(&bytes)
                .map(Some)
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// The key of the pack at `path`, its header only read: `None` without a pack of this
    /// version there.
    pub fn read_key(path: &Path) -> Option<u128> {
        use std::io::Read;
        let mut header = [0u8; HEADER];
        std::fs::File::open(path)
            .and_then(|mut f| f.read_exact(&mut header))
            .ok()?;
        key_of(&header).ok()
    }
}

/// The key from a pack's header.
fn key_of(bytes: &[u8]) -> Result<u128, String> {
    if bytes.len() < HEADER || &bytes[..4] != MAGIC {
        return Err(String::from("not a material pack"));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap_or_default());
    if version != VERSION {
        return Err(format!(
            "a material pack of version {version}, not {VERSION}"
        ));
    }
    Ok(u128::from_le_bytes(
        bytes[8..24].try_into().unwrap_or_default(),
    ))
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u32).to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    at: &'a [u8],
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let (head, rest) = self
            .at
            .split_first_chunk::<N>()
            .ok_or_else(|| String::from("cut short"))?;
        self.at = rest;
        Ok(*head)
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.take::<4>().map(u32::from_le_bytes)
    }

    fn str(&mut self) -> Result<String, String> {
        let n = self.u32()? as usize;
        if n > self.at.len() {
            return Err(String::from("cut short"));
        }
        let (s, rest) = self.at.split_at(n);
        self.at = rest;
        String::from_utf8(s.to_vec()).map_err(|_| String::from("a name not in UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pack_reads_back_as_written() {
        let pack = MaterialPack {
            key: 0x0123_4567_89ab_cdef_0011_2233_4455_6677,
            types: vec![PackedType {
                module: String::from("materials/velvet.slang"),
                type_name: String::from("VelvetMaterial"),
                layout: StructLayout {
                    name: String::from("VelvetMaterial.Params"),
                    size: 48,
                    fields: vec![
                        Field {
                            name: String::from("color"),
                            offset: 0,
                            kind: FieldKind::Float(4),
                            count: 1,
                            stride: 16,
                        },
                        Field {
                            name: String::from("pattern"),
                            offset: 16,
                            kind: FieldKind::Texture,
                            count: 2,
                            stride: 4,
                        },
                    ],
                },
                entries: vec![
                    (String::from("fragmentMain"), vec![0x0723_0203, 7, 8, 9]),
                    (String::from("fragmentMainAo"), vec![0x0723_0203; 100]),
                ],
            }],
        };
        let bytes = pack.to_bytes().unwrap();
        assert_eq!(MaterialPack::from_bytes(&bytes).unwrap(), pack);
        assert!(
            pack.find("materials/velvet.slang", "VelvetMaterial")
                .is_some()
        );
        assert!(pack.find("materials/velvet.slang", "Other").is_none());
        // The key alone from the header; a damaged or foreign file is no pack.
        let dir = std::env::temp_dir().join(format!("ofelia-mpack-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MATERIAL_PACK);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(MaterialPack::read_key(&path), Some(pack.key));
        assert_eq!(MaterialPack::load(&path).unwrap(), Some(pack));
        assert!(MaterialPack::from_bytes(&bytes[..bytes.len() - 3]).is_err());
        assert!(MaterialPack::from_bytes(b"RIFF....").is_err());
        assert_eq!(MaterialPack::load(&dir.join("none.pack")).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
