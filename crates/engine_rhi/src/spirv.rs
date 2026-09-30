//! What a compute shader expects from the code that dispatches it, read from its SPIR-V
//! (ADR 0052): the workgroup size and the size of its push constant block. Only the few
//! instructions that decide these are decoded; everything else is skipped by word count.

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
