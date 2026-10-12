//! Struct layouts as shaders read them through a pointer: a material type's parameters
//! (ADR 0092).
//!
//! Slang lays out a struct read through a pointer (`loadAligned<16>((T*)address)`) by its
//! *natural* rules: every 32-bit scalar aligned to 4 bytes, vectors and arrays to their
//! scalars, a struct to its widest field, sizes rounded up to the alignment (`float3` then
//! `float` packs into 16 bytes, a `float4` may start at byte 12). Slang's reflection API
//! offers the std140 and std430 rules but not these, so the fields and their types come
//! from reflection and the offsets from [`StructLayout`]'s own rules. A test compares them
//! with the offsets in compiled SPIR-V.

use crate::slang::{
    Functions, ReflectionType, ReflectionTypeLayout, SCALAR_FLOAT32, SCALAR_INT32, SCALAR_UINT32,
    TYPE_KIND_ARRAY, TYPE_KIND_MATRIX, TYPE_KIND_SCALAR, TYPE_KIND_STRUCT, TYPE_KIND_VECTOR,
};
use core::ffi::{CStr, c_char};

/// The name of the struct a field must have to hold a texture (`forward.slang`'s
/// `TextureRef`: a bindless index with its sampler bits).
pub const TEXTURE_REF: &str = "TextureRef";

/// What a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldKind {
    /// `float` to `float4`: the component count.
    Float(u8),
    /// `uint` to `uint4`.
    Uint(u8),
    /// `int` to `int4`.
    Int(u8),
    /// A [`TEXTURE_REF`]: 4 bytes the material table fills with a bindless texture.
    Texture,
}

impl FieldKind {
    /// Bytes of one element.
    pub fn bytes(self) -> u32 {
        match self {
            Self::Float(n) | Self::Uint(n) | Self::Int(n) => 4 * u32::from(n),
            Self::Texture => 4,
        }
    }
}

/// A field of a struct, flattened: a nested struct's fields are `outer.inner`, an array
/// of structs' are `name[i].inner`; an array of scalars, vectors or textures is one field
/// with its `count` and `stride`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    /// Its path from the struct.
    pub name: String,
    /// Bytes from the struct's start.
    pub offset: u32,
    /// What it holds.
    pub kind: FieldKind,
    /// Elements: 1 unless it is an array.
    pub count: u32,
    /// Bytes from one element to the next (arrays).
    pub stride: u32,
}

/// A struct's layout through a pointer (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StructLayout {
    /// The struct's name.
    pub name: String,
    /// Its size in bytes.
    pub size: u32,
    /// Its fields, in declaration order.
    pub fields: Vec<Field>,
}

impl StructLayout {
    /// The field named `name`.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// A type's fields relative to its start, its size and its alignment.
struct Laid {
    fields: Vec<Field>,
    size: u32,
    align: u32,
}

fn round_up(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

fn prefixed(prefix: &str, name: &str) -> String {
    match (prefix.is_empty(), name.is_empty()) {
        (true, _) => name.to_owned(),
        (false, true) => prefix.to_owned(),
        (false, false) => format!("{prefix}.{name}"),
    }
}

/// # Safety
///
/// `text` is null or a NUL-terminated string.
unsafe fn string(text: *const c_char) -> String {
    if text.is_null() {
        String::new()
    } else {
        // SAFETY: guaranteed by the caller.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }
}

/// The natural layout of the type `layout` describes, its fields named under `path`.
///
/// # Safety
///
/// `layout` comes from a live reflection.
pub(crate) unsafe fn natural_layout(
    f: &Functions,
    layout: *mut ReflectionTypeLayout,
    path: &str,
) -> Result<StructLayoutParts, String> {
    // SAFETY: guaranteed by the caller.
    let laid = unsafe { lay_out(f, layout, path) }?;
    Ok(StructLayoutParts {
        size: round_up(laid.size, laid.align),
        fields: laid.fields,
    })
}

/// A struct's size and fields (its name is the caller's).
pub(crate) struct StructLayoutParts {
    pub(crate) size: u32,
    pub(crate) fields: Vec<Field>,
}

/// # Safety
///
/// `ty` comes from a live reflection.
unsafe fn leaf(
    f: &Functions,
    ty: *mut ReflectionType,
    count: u8,
    path: &str,
) -> Result<FieldKind, String> {
    // SAFETY: guaranteed by the caller.
    let scalar = unsafe { (f.type_scalar)(ty) };
    Ok(match scalar {
        SCALAR_FLOAT32 => FieldKind::Float(count),
        SCALAR_UINT32 => FieldKind::Uint(count),
        SCALAR_INT32 => FieldKind::Int(count),
        _ => {
            // SAFETY: as above.
            let name = unsafe { string((f.type_name)(ty)) };
            return Err(format!(
                "`{path}`: `{name}` is not supported in material parameters (32-bit float, uint and int scalars and vectors, {TEXTURE_REF}, arrays and structs of them)"
            ));
        }
    })
}

/// # Safety
///
/// `layout` comes from a live reflection.
unsafe fn lay_out(
    f: &Functions,
    layout: *mut ReflectionTypeLayout,
    path: &str,
) -> Result<Laid, String> {
    // SAFETY: guaranteed by the caller; every pointer below comes from the same reflection.
    unsafe {
        let ty = (f.type_layout_type)(layout);
        if ty.is_null() {
            return Err(format!("`{path}`: Slang gave no type"));
        }
        let single = |kind: FieldKind| Laid {
            fields: vec![Field {
                name: path.to_owned(),
                offset: 0,
                kind,
                count: 1,
                stride: kind.bytes(),
            }],
            size: kind.bytes(),
            align: 4,
        };
        match (f.type_kind)(ty) {
            TYPE_KIND_SCALAR => Ok(single(leaf(f, ty, 1, path)?)),
            TYPE_KIND_VECTOR => {
                let count = (f.type_element_count)(ty);
                let count = u8::try_from(count)
                    .ok()
                    .filter(|n| (1..=4).contains(n))
                    .ok_or_else(|| format!("`{path}`: a vector of {count}"))?;
                Ok(single(leaf(f, ty, count, path)?))
            }
            TYPE_KIND_STRUCT if string((f.type_name)(ty)) == TEXTURE_REF => {
                Ok(single(FieldKind::Texture))
            }
            TYPE_KIND_STRUCT => {
                let mut fields = Vec::new();
                let mut cursor = 0;
                let mut align = 1;
                for i in 0..(f.type_layout_field_count)(layout) {
                    let field = (f.type_layout_field)(layout, i);
                    let name = string((f.variable_name)((f.variable_layout_variable)(field)));
                    let child = lay_out(
                        f,
                        (f.variable_layout_type_layout)(field),
                        &prefixed(path, &name),
                    )?;
                    let offset = round_up(cursor, child.align);
                    fields.extend(child.fields.into_iter().map(|mut c| {
                        c.offset += offset;
                        c
                    }));
                    cursor = offset + child.size;
                    align = align.max(child.align);
                }
                Ok(Laid {
                    fields,
                    size: round_up(cursor, align),
                    align,
                })
            }
            TYPE_KIND_ARRAY => {
                let count = u32::try_from((f.type_element_count)(ty))
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| format!("`{path}`: arrays need a size"))?;
                let element = (f.type_layout_element)(layout);
                if element.is_null() {
                    return Err(format!("`{path}`: Slang gave no element type"));
                }
                let child = lay_out(f, element, "")?;
                let stride = round_up(child.size, child.align);
                let fields = match child.fields.as_slice() {
                    // Scalars, vectors and textures: one field.
                    [one] if one.name.is_empty() && one.count == 1 => vec![Field {
                        name: path.to_owned(),
                        offset: 0,
                        kind: one.kind,
                        count,
                        stride,
                    }],
                    _ => (0..count)
                        .flat_map(|i| {
                            child.fields.iter().map(move |c| Field {
                                name: format!(
                                    "{path}[{i}]{}",
                                    if c.name.is_empty() {
                                        String::new()
                                    } else {
                                        format!(".{}", c.name)
                                    }
                                ),
                                offset: c.offset + i * stride,
                                ..c.clone()
                            })
                        })
                        .collect(),
                };
                Ok(Laid {
                    fields,
                    size: stride * count,
                    align: child.align,
                })
            }
            TYPE_KIND_MATRIX => Err(format!(
                "`{path}`: matrices are not supported in material parameters yet"
            )),
            kind => {
                let name = string((f.type_name)(ty));
                Err(format!(
                    "`{path}`: `{name}` (Slang type kind {kind}) is not supported in material parameters"
                ))
            }
        }
    }
}
