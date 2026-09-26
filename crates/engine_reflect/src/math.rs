//! Descriptions of the engine's math types (`engine_core::math`, glam).

use crate::{Attrs, Field, Reflect, TypeInfo, TypeKind};
use core::mem::{align_of, offset_of, size_of};
use engine_core::math::{IVec2, IVec3, Mat4, Quat, UVec2, Vec2, Vec3, Vec3A, Vec4};

const fn f(name: &'static str, offset: usize, ty: &'static TypeInfo) -> Field {
    Field {
        name,
        offset,
        ty,
        attrs: Attrs::NONE,
    }
}

const F32: &TypeInfo = <f32 as Reflect>::TYPE_INFO;
const I32: &TypeInfo = <i32 as Reflect>::TYPE_INFO;
const U32: &TypeInfo = <u32 as Reflect>::TYPE_INFO;

macro_rules! vector {
    ($t:ty, $name:literal, $elem:expr, [$($field:ident),+]) => {
        impl Reflect for $t {
            const TYPE_INFO: &'static TypeInfo = &TypeInfo {
                name: $name,
                size: size_of::<$t>(),
                align: align_of::<$t>(),
                kind: TypeKind::Struct(&[$(f(stringify!($field), offset_of!($t, $field), $elem)),+]),
                attrs: Attrs::NONE,
            };
        }
    };
}

vector!(Vec2, "Vec2", F32, [x, y]);
vector!(Vec3, "Vec3", F32, [x, y, z]);
vector!(IVec2, "IVec2", I32, [x, y]);
vector!(IVec3, "IVec3", I32, [x, y, z]);
vector!(UVec2, "UVec2", U32, [x, y]);

// SIMD-backed types keep their fields private: described by layout (checked by a test).
// Vec4 and Quat are x, y, z, w; Vec3A is three floats plus padding; Mat4 is four columns.
impl Reflect for Vec3A {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "Vec3A",
        size: size_of::<Vec3A>(),
        align: align_of::<Vec3A>(),
        kind: TypeKind::Struct(&[f("x", 0, F32), f("y", 4, F32), f("z", 8, F32)]),
        attrs: Attrs::NONE,
    };
}

impl Reflect for Vec4 {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "Vec4",
        size: size_of::<Vec4>(),
        align: align_of::<Vec4>(),
        kind: TypeKind::Struct(&[
            f("x", 0, F32),
            f("y", 4, F32),
            f("z", 8, F32),
            f("w", 12, F32),
        ]),
        attrs: Attrs::NONE,
    };
}

impl Reflect for Quat {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "Quat",
        size: size_of::<Quat>(),
        align: align_of::<Quat>(),
        kind: TypeKind::Struct(&[
            f("x", 0, F32),
            f("y", 4, F32),
            f("z", 8, F32),
            f("w", 12, F32),
        ]),
        attrs: Attrs::NONE,
    };
}

impl Reflect for Mat4 {
    const TYPE_INFO: &'static TypeInfo = &TypeInfo {
        name: "Mat4",
        size: size_of::<Mat4>(),
        align: align_of::<Mat4>(),
        kind: TypeKind::Array {
            element: <[f32; 4] as Reflect>::TYPE_INFO,
            len: 4,
        },
        attrs: Attrs::NONE,
    };
}
