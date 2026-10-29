//! What a compute shader expects from the code that dispatches it, read from its SPIR-V
//! (ADR 0052): the workgroup size and the size of its push constant block. Only the few
//! instructions that decide these are decoded; everything else is skipped by word count.
//! And how much code an entry point reaches ([`code_size`], a material's cost in the
//! editor, E10-4f).

use std::collections::HashMap;

/// A compute shader's interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaderInterface {
    /// `numthreads`: invocations per workgroup along x, y and z (`None` when the module
    /// declares none, as for graphics stages).
    pub local_size: Option<[u32; 3]>,
    /// Bytes of the push constant block (0 without one): the highest member offset plus
    /// that member's size.
    pub push_constant_bytes: u32,
}

const MAGIC: u32 = 0x0723_0203;

// Opcodes.
const OP_EXECUTION_MODE: u32 = 16;
const OP_TYPE_BOOL: u32 = 20;
const OP_TYPE_INT: u32 = 21;
const OP_TYPE_FLOAT: u32 = 22;
const OP_TYPE_VECTOR: u32 = 23;
const OP_TYPE_MATRIX: u32 = 24;
const OP_TYPE_ARRAY: u32 = 28;
const OP_TYPE_STRUCT: u32 = 30;
const OP_TYPE_POINTER: u32 = 32;
const OP_CONSTANT: u32 = 43;
const OP_CONSTANT_COMPOSITE: u32 = 44;
const OP_SPEC_CONSTANT: u32 = 50;
const OP_SPEC_CONSTANT_COMPOSITE: u32 = 51;
const OP_VARIABLE: u32 = 59;
const OP_DECORATE: u32 = 71;
const OP_MEMBER_DECORATE: u32 = 72;
const OP_EXECUTION_MODE_ID: u32 = 331;
const OP_LINE: u32 = 8;
const OP_EXT_INST_IMPORT: u32 = 11;
const OP_EXT_INST: u32 = 12;
const OP_ENTRY_POINT: u32 = 15;
const OP_FUNCTION: u32 = 54;
const OP_FUNCTION_PARAMETER: u32 = 55;
const OP_FUNCTION_END: u32 = 56;
const OP_FUNCTION_CALL: u32 = 57;
const OP_LABEL: u32 = 248;
const OP_NO_LINE: u32 = 317;

// Operands.
const MODE_LOCAL_SIZE: u32 = 17;
const MODE_LOCAL_SIZE_ID: u32 = 38;
const DECORATION_ARRAY_STRIDE: u32 = 6;
const DECORATION_MATRIX_STRIDE: u32 = 7;
const DECORATION_BUILT_IN: u32 = 11;
const DECORATION_OFFSET: u32 = 35;
const BUILT_IN_WORKGROUP_SIZE: u32 = 25;
const STORAGE_PUSH_CONSTANT: u32 = 9;
const STORAGE_PHYSICAL_STORAGE_BUFFER: u32 = 5349;

#[derive(Debug, Clone)]
enum Type {
    Scalar(u32),
    Vector(u32, u32),
    Matrix(u32, u32),
    Array(u32, u32),
    Struct(Vec<u32>),
    Pointer(u32, u32),
}

#[derive(Default)]
struct Module {
    types: HashMap<u32, Type>,
    constants: HashMap<u32, u32>,
    composites: HashMap<u32, Vec<u32>>,
    array_strides: HashMap<u32, u32>,
    /// `(struct, member)` → offset, matrix stride.
    offsets: HashMap<(u32, u32), u32>,
    matrix_strides: HashMap<(u32, u32), u32>,
    workgroup_size_id: Option<u32>,
    local_size: Option<[u32; 3]>,
    local_size_ids: Option<[u32; 3]>,
    push_pointer: Option<u32>,
}

impl Module {
    /// Size of type `id` in bytes, following strides and offsets; `None` if unknown.
    fn size(&self, id: u32, depth: u32) -> Option<u32> {
        if depth > 16 {
            return None;
        }
        Some(match self.types.get(&id)? {
            Type::Scalar(bytes) => *bytes,
            Type::Vector(component, count) => self.size(*component, depth + 1)? * count,
            Type::Matrix(column, count) => self.size(*column, depth + 1)? * count,
            Type::Array(element, length) => {
                let length = *self.constants.get(length)?;
                let stride = match self.array_strides.get(&id) {
                    Some(&stride) => stride,
                    None => self.size(*element, depth + 1)?,
                };
                stride * length
            }
            Type::Struct(members) => {
                let mut end = 0;
                for (m, &member) in members.iter().enumerate() {
                    let key = (id, m as u32);
                    let offset = *self.offsets.get(&key)?;
                    let size = match (self.types.get(&member)?, self.matrix_strides.get(&key)) {
                        (Type::Matrix(_, columns), Some(stride)) => columns * stride,
                        _ => self.size(member, depth + 1)?,
                    };
                    end = end.max(offset + size);
                }
                end
            }
            Type::Pointer(class, _) if *class == STORAGE_PHYSICAL_STORAGE_BUFFER => 8,
            Type::Pointer(..) => return None,
        })
    }
}

/// Reads a compute shader's interface from its SPIR-V words; `None` if the module is
/// malformed or uses a push constant type this reader does not size.
pub fn reflect(words: &[u32]) -> Option<ShaderInterface> {
    if words.len() < 5 || words[0] != MAGIC {
        return None;
    }
    let mut m = Module::default();
    let mut i = 5;
    while i < words.len() {
        let count = (words[i] >> 16) as usize;
        let op = words[i] & 0xFFFF;
        if count == 0 || i + count > words.len() {
            return None;
        }
        let w = &words[i + 1..i + count];
        let arg = |k: usize| w.get(k).copied();
        match op {
            OP_EXECUTION_MODE if arg(1) == Some(MODE_LOCAL_SIZE) => {
                m.local_size = Some([arg(2)?, arg(3)?, arg(4)?]);
            }
            OP_EXECUTION_MODE_ID if arg(1) == Some(MODE_LOCAL_SIZE_ID) => {
                m.local_size_ids = Some([arg(2)?, arg(3)?, arg(4)?]);
            }
            OP_TYPE_BOOL => {
                m.types.insert(arg(0)?, Type::Scalar(4));
            }
            OP_TYPE_INT | OP_TYPE_FLOAT => {
                m.types.insert(arg(0)?, Type::Scalar(arg(1)? / 8));
            }
            OP_TYPE_VECTOR => {
                m.types.insert(arg(0)?, Type::Vector(arg(1)?, arg(2)?));
            }
            OP_TYPE_MATRIX => {
                m.types.insert(arg(0)?, Type::Matrix(arg(1)?, arg(2)?));
            }
            OP_TYPE_ARRAY => {
                m.types.insert(arg(0)?, Type::Array(arg(1)?, arg(2)?));
            }
            OP_TYPE_STRUCT => {
                m.types.insert(arg(0)?, Type::Struct(w.get(1..)?.to_vec()));
            }
            OP_TYPE_POINTER => {
                m.types.insert(arg(0)?, Type::Pointer(arg(1)?, arg(2)?));
            }
            OP_CONSTANT | OP_SPEC_CONSTANT => {
                // The low word (sizes and counts are 32-bit).
                m.constants.insert(arg(1)?, arg(2)?);
            }
            OP_CONSTANT_COMPOSITE | OP_SPEC_CONSTANT_COMPOSITE => {
                m.composites.insert(arg(1)?, w.get(2..)?.to_vec());
            }
            OP_VARIABLE if arg(2) == Some(STORAGE_PUSH_CONSTANT) => {
                m.push_pointer = Some(arg(0)?);
            }
            OP_DECORATE => match arg(1) {
                Some(DECORATION_ARRAY_STRIDE) => {
                    m.array_strides.insert(arg(0)?, arg(2)?);
                }
                Some(DECORATION_BUILT_IN) if arg(2) == Some(BUILT_IN_WORKGROUP_SIZE) => {
                    m.workgroup_size_id = Some(arg(0)?);
                }
                _ => {}
            },
            OP_MEMBER_DECORATE => match arg(2) {
                Some(DECORATION_OFFSET) => {
                    m.offsets.insert((arg(0)?, arg(1)?), arg(3)?);
                }
                Some(DECORATION_MATRIX_STRIDE) => {
                    m.matrix_strides.insert((arg(0)?, arg(1)?), arg(3)?);
                }
                _ => {}
            },
            _ => {}
        }
        i += count;
    }

    // WorkgroupSize wins over the execution modes (SPIR-V specification, 3.20).
    let local_size = match (m.workgroup_size_id, m.local_size_ids, m.local_size) {
        (Some(id), _, _) => {
            let parts = m.composites.get(&id)?;
            let value = |k: usize| parts.get(k).and_then(|c| m.constants.get(c)).copied();
            Some([value(0)?, value(1)?, value(2)?])
        }
        (None, Some(ids), _) => Some([
            *m.constants.get(&ids[0])?,
            *m.constants.get(&ids[1])?,
            *m.constants.get(&ids[2])?,
        ]),
        (None, None, local_size) => local_size,
    };
    let push_constant_bytes = match m.push_pointer {
        None => 0,
        Some(pointer) => match m.types.get(&pointer)? {
            Type::Pointer(_, pointee) => m.size(*pointee, 0)?,
            _ => return None,
        },
    };
    Some(ShaderInterface {
        local_size,
        push_constant_bytes,
    })
}

/// How much code an entry point reaches, as its SPIR-V says (E10-4f, a material's cost):
/// the instructions of the functions it reaches, each counted once (debug lines and
/// non-semantic information left out), and the image reads among them. Counts in the
/// code, not of what runs: a loop runs its body more often, a branch less.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeSize {
    /// Instructions in the bodies of the functions reached.
    pub instructions: u32,
    /// Image samples, fetches and gathers among them (sparse ones too).
    pub samples: u32,
}

/// Whether instruction `op` reads an image: a sample, a fetch or a gather.
fn reads_image(op: u32) -> bool {
    (87..=97).contains(&op) || (305..=315).contains(&op)
}

/// The bytes of the literal string the words start with (up to its nul).
fn literal(words: &[u32]) -> Vec<u8> {
    words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .take_while(|&b| b != 0)
        .collect()
}

/// The code entry point `entry` of a SPIR-V module reaches ([`CodeSize`]); `None` if the
/// module is malformed or has no such entry point.
pub fn code_size(words: &[u32], entry: &str) -> Option<CodeSize> {
    if words.len() < 5 || words[0] != MAGIC {
        return None;
    }
    let mut root = None;
    let mut non_semantic = Vec::new();
    // By function: its instructions, its image reads, the functions it calls.
    let mut functions: HashMap<u32, (u32, u32, Vec<u32>)> = HashMap::new();
    let mut current = None;
    let mut i = 5;
    while i < words.len() {
        let count = (words[i] >> 16) as usize;
        let op = words[i] & 0xFFFF;
        if count == 0 || i + count > words.len() {
            return None;
        }
        let w = &words[i + 1..i + count];
        match op {
            OP_ENTRY_POINT if w.len() > 2 && literal(&w[2..]) == entry.as_bytes() => {
                root = Some(w[1]);
            }
            OP_EXT_INST_IMPORT if w.len() > 1 && literal(&w[1..]).starts_with(b"NonSemantic.") => {
                non_semantic.push(w[0]);
            }
            OP_FUNCTION => {
                let id = *w.get(1)?;
                functions.entry(id).or_default();
                current = Some(id);
            }
            OP_FUNCTION_END => current = None,
            OP_FUNCTION_PARAMETER | OP_LABEL | OP_LINE | OP_NO_LINE => {}
            OP_EXT_INST if w.get(2).is_some_and(|set| non_semantic.contains(set)) => {}
            _ => {
                if let Some(f) = current.and_then(|id| functions.get_mut(&id)) {
                    f.0 += 1;
                    f.1 += u32::from(reads_image(op));
                    if op == OP_FUNCTION_CALL {
                        f.2.push(*w.get(2)?);
                    }
                }
            }
        }
        i += count;
    }
    let mut reached = vec![root?];
    let mut size = CodeSize::default();
    let mut k = 0;
    while let Some(&id) = reached.get(k) {
        k += 1;
        let Some((instructions, samples, calls)) = functions.get(&id) else {
            continue;
        };
        size.instructions += instructions;
        size.samples += samples;
        for &callee in calls {
            if !reached.contains(&callee) {
                reached.push(callee);
            }
        }
    }
    Some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module: header, then instructions given as `(opcode, operands)`.
    fn module(instructions: &[(u32, &[u32])]) -> Vec<u32> {
        let mut words = vec![MAGIC, 0x0001_0600, 0, 100, 0];
        for (op, operands) in instructions {
            words.push(((operands.len() as u32 + 1) << 16) | op);
            words.extend_from_slice(operands);
        }
        words
    }

    #[test]
    fn reads_the_workgroup_size_and_the_push_constant_block() {
        // struct { uint a; float4 b (offset 16); uint64 pointer (offset 32); float c[3]
        // (offset 40, stride 4) } → 52 bytes; numthreads(8, 4, 1).
        let words = module(&[
            (OP_EXECUTION_MODE, &[1, MODE_LOCAL_SIZE, 8, 4, 1]),
            (OP_MEMBER_DECORATE, &[20, 0, DECORATION_OFFSET, 0]),
            (OP_MEMBER_DECORATE, &[20, 1, DECORATION_OFFSET, 16]),
            (OP_MEMBER_DECORATE, &[20, 2, DECORATION_OFFSET, 32]),
            (OP_MEMBER_DECORATE, &[20, 3, DECORATION_OFFSET, 40]),
            (OP_DECORATE, &[16, DECORATION_ARRAY_STRIDE, 4]),
            (OP_TYPE_INT, &[10, 32, 0]),
            (OP_TYPE_FLOAT, &[11, 32]),
            (OP_TYPE_VECTOR, &[12, 11, 4]),
            (OP_TYPE_POINTER, &[13, STORAGE_PHYSICAL_STORAGE_BUFFER, 11]),
            (OP_CONSTANT, &[10, 15, 3]),
            (OP_TYPE_ARRAY, &[16, 11, 15]),
            (OP_TYPE_STRUCT, &[20, 10, 12, 13, 16]),
            (OP_TYPE_POINTER, &[21, STORAGE_PUSH_CONSTANT, 20]),
            (OP_VARIABLE, &[21, 22, STORAGE_PUSH_CONSTANT]),
        ]);
        assert_eq!(
            reflect(&words),
            Some(ShaderInterface {
                local_size: Some([8, 4, 1]),
                push_constant_bytes: 52,
            })
        );
    }

    #[test]
    fn workgroup_size_from_ids_and_builtin_constants() {
        let by_id = module(&[
            (OP_EXECUTION_MODE_ID, &[1, MODE_LOCAL_SIZE_ID, 30, 31, 31]),
            (OP_TYPE_INT, &[10, 32, 0]),
            (OP_SPEC_CONSTANT, &[10, 30, 64]),
            (OP_CONSTANT, &[10, 31, 1]),
        ]);
        let size = reflect(&by_id).and_then(|i| i.local_size);
        assert_eq!(size, Some([64, 1, 1]));
        let builtin = module(&[
            (OP_EXECUTION_MODE, &[1, MODE_LOCAL_SIZE, 1, 1, 1]),
            (
                OP_DECORATE,
                &[40, DECORATION_BUILT_IN, BUILT_IN_WORKGROUP_SIZE],
            ),
            (OP_TYPE_INT, &[10, 32, 0]),
            (OP_CONSTANT, &[10, 30, 16]),
            (OP_CONSTANT, &[10, 31, 2]),
            (OP_CONSTANT_COMPOSITE, &[12, 40, 30, 30, 31]),
        ]);
        let size = reflect(&builtin).and_then(|i| i.local_size);
        assert_eq!(size, Some([16, 16, 2]));
    }

    /// A literal string's words (nul-terminated, padded).
    fn string(s: &str) -> Vec<u32> {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    /// E10-4f: an entry point's code is the functions it reaches, each once, without debug
    /// lines and non-semantic instructions; image samples and fetches counted among them.
    #[test]
    fn an_entry_points_code_is_what_it_reaches() {
        const OP_RETURN: u32 = 253;
        const OP_SAMPLE: u32 = 87;
        const OP_FETCH: u32 = 95;
        const OP_FADD: u32 = 129;
        let with = |head: &[u32], tail: Vec<u32>| [head, &tail[..]].concat();
        let debug = with(&[1], string("NonSemantic.Shader.DebugInfo.100"));
        let glsl = with(&[2], string("GLSL.std.450"));
        let main = with(&[4, 10], string("main"));
        let other = with(&[4, 30], string("other"));
        let words = module(&[
            (OP_EXT_INST_IMPORT, &debug),
            (OP_EXT_INST_IMPORT, &glsl),
            (OP_ENTRY_POINT, &main),
            (OP_ENTRY_POINT, &other),
            // main: a sample, a GLSL instruction, a debug one (left out), a call, twice.
            (OP_FUNCTION, &[3, 10, 0, 5]),
            (OP_LABEL, &[11]),
            (OP_LINE, &[40, 1, 1]),
            (OP_SAMPLE, &[6, 12, 13, 14]),
            (OP_EXT_INST, &[7, 15, 2, 4, 12]),
            (OP_EXT_INST, &[3, 16, 1, 1]),
            (OP_FUNCTION_CALL, &[3, 17, 20]),
            (OP_FUNCTION_CALL, &[3, 18, 20]),
            (OP_RETURN, &[]),
            (OP_FUNCTION_END, &[]),
            // A helper: a parameter, a fetch.
            (OP_FUNCTION, &[3, 20, 0, 5]),
            (OP_FUNCTION_PARAMETER, &[6, 21]),
            (OP_LABEL, &[22]),
            (OP_FETCH, &[6, 23, 21, 24]),
            (OP_RETURN, &[]),
            (OP_FUNCTION_END, &[]),
            // The other entry point's: not main's.
            (OP_FUNCTION, &[3, 30, 0, 5]),
            (OP_LABEL, &[31]),
            (OP_FADD, &[6, 32, 33, 34]),
            (OP_FADD, &[6, 35, 32, 34]),
            (OP_RETURN, &[]),
            (OP_FUNCTION_END, &[]),
        ]);
        assert_eq!(
            code_size(&words, "main"),
            Some(CodeSize {
                instructions: 7,
                samples: 2,
            })
        );
        assert_eq!(
            code_size(&words, "other"),
            Some(CodeSize {
                instructions: 3,
                samples: 0,
            })
        );
        assert_eq!(code_size(&words, "none"), None);
        assert_eq!(code_size(&words[..20], "main"), None, "cut short");
    }

    #[test]
    fn malformed_modules_are_refused() {
        assert_eq!(reflect(&[]), None);
        assert_eq!(reflect(&[0; 5]), None);
        let mut words = module(&[(OP_TYPE_INT, &[10, 32, 0])]);
        words[5] = 0; // a zero word count
        assert_eq!(reflect(&words), None);
        let words = module(&[(OP_TYPE_INT, &[10, 32, 0])]);
        assert_eq!(reflect(&words[..6]), None); // cut short
    }
}
