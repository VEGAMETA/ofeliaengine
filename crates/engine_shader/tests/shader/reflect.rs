//! Struct layouts through pointers (ADR 0092): the offsets `Compiler::reflect_struct`
//! computes must be the ones Slang compiles (see `common` for when these run).

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use crate::common;

use common::{slang, temp_dir};

use engine_shader::{CompileOptions, Compiler, EntryPoint, FieldKind, Stage};
use std::collections::HashMap;

/// A probe of every kind of field, read through a pointer by an entry point.
const PROBE: &str = r#"
struct TextureRef { uint packed; };
struct Inner { float2 uv; uint flags; };
struct Probe
{
    float3 a;
    float b;
    float3 c;
    float2 d;
    float e;
    TextureRef t;
    float4 f;
    float g[3];
    Inner inner;
    int3 h;
    Inner pairs[2];
    uint u;
};
struct ProbeMaterial { typealias Params = Probe; };
struct Push { uint64_t address; };
[[vk::push_constant]] Push push;

[shader("compute")]
[numthreads(1, 1, 1)]
void probeMain()
{
    Probe p = loadAligned<16>((Probe*)push.address);
    p.a += 1.0;
    *(Probe*)push.address = p;
}
"#;

/// What compiled SPIR-V says of `<name>_natural` structs: member offsets by member name,
/// and the pointer stride of `name` (its size).
struct SpirvLayouts {
    /// Struct name → member name → offset.
    members: HashMap<String, HashMap<String, u32>>,
    /// Struct name → array stride of a pointer to it.
    strides: HashMap<String, u32>,
}

fn words_string(words: &[u32]) -> String {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn spirv_layouts(spirv: &[u32]) -> SpirvLayouts {
    let mut names = HashMap::new();
    let mut member_names: HashMap<(u32, u32), String> = HashMap::new();
    let mut offsets: HashMap<(u32, u32), u32> = HashMap::new();
    let mut pointer_strides: HashMap<u32, u32> = HashMap::new();
    let mut pointees: HashMap<u32, u32> = HashMap::new();
    let mut i = 5;
    while i < spirv.len() {
        let (op, count) = (spirv[i] & 0xFFFF, (spirv[i] >> 16) as usize);
        let args = &spirv[i + 1..i + count];
        match op {
            // OpName: target, name.
            5 => {
                names.insert(args[0], words_string(&args[1..]));
            }
            // OpMemberName: struct, member, name.
            6 => {
                member_names.insert((args[0], args[1]), words_string(&args[2..]));
            }
            // OpDecorate target ArrayStride (6) stride.
            71 if args[1] == 6 => {
                pointer_strides.insert(args[0], args[2]);
            }
            // OpMemberDecorate struct member Offset (35) offset.
            72 if args[2] == 35 => {
                offsets.insert((args[0], args[1]), args[3]);
            }
            // OpTypePointer: result, storage class, pointee.
            32 => {
                pointees.insert(args[0], args[2]);
            }
            _ => {}
        }
        i += count.max(1);
    }
    let mut members: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for ((id, member), offset) in &offsets {
        let (Some(name), Some(member_name)) = (names.get(id), member_names.get(&(*id, *member)))
        else {
            continue;
        };
        if let Some(name) = name.strip_suffix("_natural") {
            members
                .entry(name.to_owned())
                .or_default()
                .insert(member_name.clone(), *offset);
        }
    }
    let strides = pointer_strides
        .iter()
        .filter_map(|(pointer, stride)| {
            let name = names.get(pointees.get(pointer)?)?;
            Some((name.strip_suffix("_natural")?.to_owned(), *stride))
        })
        .collect();
    SpirvLayouts { members, strides }
}

#[test]
fn natural_layouts_match_the_compiled_spirv() {
    let Some(library) = slang() else { return };
    let dir = temp_dir("reflect");
    let path = dir.join("probe.slang");
    std::fs::write(&path, PROBE).unwrap();
    let mut compiler = Compiler::new(library).unwrap();
    let options = CompileOptions {
        optimize: true,
        ..CompileOptions::default()
    };
    let compiled = compiler
        .compile_file(
            &path,
            &[EntryPoint::new("probeMain", Stage::Compute)],
            &options,
        )
        .unwrap();
    let spirv = spirv_layouts(&compiled.entries[0].spirv);
    let layout = compiler
        .reflect_struct(&path, &options, "ProbeMaterial.Params")
        .unwrap();
    println!("{layout:#?}");
    assert_eq!(layout.name, "Probe");
    assert_eq!(Some(&layout.size), spirv.strides.get("Probe"), "size");

    // Every top-level member: its first flattened field starts at the member's offset.
    let probe = &spirv.members["Probe"];
    for (member, &offset) in probe {
        let first = layout
            .fields
            .iter()
            .find(|f| {
                f.name == *member
                    || f.name.starts_with(&format!("{member}."))
                    || f.name.starts_with(&format!("{member}["))
            })
            .unwrap_or_else(|| panic!("no field for `{member}`"));
        assert_eq!(first.offset, offset, "`{member}`");
    }
    // Nested: `inner.flags` sits at `inner` plus `Inner.flags`.
    let inner = &spirv.members["Inner"];
    assert_eq!(
        layout.field("inner.flags").unwrap().offset,
        probe["inner"] + inner["flags"]
    );
    assert_eq!(
        layout.field("pairs[1].uv").unwrap().offset,
        probe["pairs"] + spirv.strides.get("Inner").copied().unwrap_or(12) + inner["uv"]
    );

    // Kinds.
    assert_eq!(layout.field("a").unwrap().kind, FieldKind::Float(3));
    assert_eq!(layout.field("t").unwrap().kind, FieldKind::Texture);
    assert_eq!(layout.field("h").unwrap().kind, FieldKind::Int(3));
    assert_eq!(layout.field("u").unwrap().kind, FieldKind::Uint(1));
    let g = layout.field("g").unwrap();
    assert_eq!((g.kind, g.count, g.stride), (FieldKind::Float(1), 3, 4));
}

#[test]
fn unsupported_fields_are_named() {
    let Some(library) = slang() else { return };
    let dir = temp_dir("reflect_unsupported");
    let mut compiler = Compiler::new(library).unwrap();
    for (field, expect) in [
        ("double d;", "`d`"),
        ("bool b;", "`b`"),
        ("float4x4 m;", "matrices"),
    ] {
        let path = dir.join("bad.slang");
        std::fs::write(&path, format!("struct Bad {{ float x; {field} }};")).unwrap();
        let error = compiler
            .reflect_struct(&path, &CompileOptions::default(), "Bad")
            .unwrap_err()
            .to_string();
        assert!(error.contains(expect), "{field}: {error}");
    }
    let path = dir.join("bad.slang");
    let error = compiler
        .reflect_struct(&path, &CompileOptions::default(), "Missing")
        .unwrap_err()
        .to_string();
    assert!(error.contains("no type `Missing`"), "{error}");
}
