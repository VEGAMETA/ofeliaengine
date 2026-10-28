//! Functions a module marks with an attribute (ADR 0123's E10-3g-3: `[Node]` functions, the
//! material graph's nodes written in Slang), found by name in the source and read from
//! Slang's reflection.
//!
//! Slang's reflection finds a function by its name, but lists a module's declarations only
//! through a COM interface (`IModule::getModuleReflection`) the flat binding does not reach
//! (ADR 0008). So the names come from the source: each function declared after a mark of
//! the attribute ([`marked_names`]); the reflection then confirms the mark and reads the
//! signature ([`crate::Compiler::marked_functions`]). A name the scan finds where Slang sees
//! no such mark is passed over.

use crate::reflect::{FieldKind, TEXTURE_REF, string};
use crate::slang::{
    Functions, MODIFIER_INOUT, MODIFIER_OUT, Reflection, ReflectionType, SCALAR_FLOAT32,
    SCALAR_INT32, SCALAR_UINT32, Session, TYPE_KIND_SCALAR, TYPE_KIND_STRUCT, TYPE_KIND_VECTOR,
};
use std::ffi::CString;

/// A function's parameter or result type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueType {
    /// A 32-bit float, uint or int scalar or vector, or a texture reference.
    Field(FieldKind),
    /// Any other type, by its name (`MaterialContext`, `void`).
    Named(String),
}

/// How a parameter passes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// In (Slang's default).
    In,
    /// `out`.
    Out,
    /// `inout`.
    InOut,
}

/// A parameter of a [`MarkedFunction`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Parameter {
    /// Its name.
    pub name: String,
    /// Its type.
    pub ty: ValueType,
    /// In, out or both.
    pub direction: Direction,
}

/// A function a module marks with an attribute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkedFunction {
    /// Its name.
    pub name: String,
    /// The mark's string arguments, in order (`[Node("Triplanar", "Mapping")]`).
    pub arguments: Vec<String>,
    /// Its parameters, in order.
    pub parameters: Vec<Parameter>,
    /// What it returns.
    pub result: ValueType,
}

/// The names of the functions `source` declares after a mark of `attribute` (`[Node]`,
/// `[Node("Triplanar")]`, with other attributes beside it), in the order they come, each
/// once. Comments are skipped; a mark on anything but a function is passed over.
pub fn marked_names(source: &str, attribute: &str) -> Vec<String> {
    let text = without_comments(source);
    let mut names: Vec<String> = Vec::new();
    let mut at = 0;
    while let Some(found) = text[at..].find('[') {
        let open = at + found;
        at = open + 1;
        let inner = text[open + 1..].trim_start();
        let Some(after) = inner.strip_prefix(attribute) else {
            continue;
        };
        if after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            continue;
        }
        // The end of its attribute block, and of any attributes after it.
        let Some(mut end) = closing(&text, open) else {
            break;
        };
        loop {
            let next = text[end..].trim_start();
            if !next.starts_with('[') {
                break;
            }
            match closing(&text, text.len() - next.len()) {
                Some(e) => end = e,
                None => break,
            }
        }
        at = end;
        // The declaration: the name right before its parameters' parenthesis.
        let declaration = &text[end..];
        let Some(stop) = declaration.find(['(', ';', '{', '=']) else {
            continue;
        };
        if !declaration[stop..].starts_with('(') {
            continue;
        }
        let head = declaration[..stop].trim_end();
        let start = head
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let name = &head[start..];
        if !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_owned());
        }
    }
    names
}

/// Just after the `]` closing the `[` at `open`, across nested brackets and string
/// literals.
fn closing(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let (mut quoted, mut escaped) = (false, false);
    for (i, c) in text[open..].char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open + i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// `source` with its comments blanked (string literals kept as they are).
fn without_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                quoted = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                quoted = true;
                out.push(c);
            }
            ('/', Some('/')) => while chars.next_if(|&n| n != '\n').is_some() {},
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for next in chars.by_ref() {
                    if last == '*' && next == '/' {
                        break;
                    }
                    last = next;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// The functions named `names` that carry `attribute`, read from `reflection` (others,
/// and overloaded names, are passed over).
///
/// # Safety
///
/// `reflection` is a live request's reflection and `session` the session it was made in.
pub(crate) unsafe fn read_marked(
    f: &Functions,
    reflection: *mut Reflection,
    session: *mut Session,
    names: &[String],
    attribute: &str,
) -> Vec<MarkedFunction> {
    let mark_name = CString::new(attribute.replace('\0', "")).unwrap_or_default();
    let mut found = Vec::new();
    for name in names {
        let c_name = CString::new(name.replace('\0', "")).unwrap_or_default();
        // SAFETY: every pointer comes from `reflection`, alive per the caller; strings are
        // NUL-terminated or come with their lengths.
        unsafe {
            let function = (f.find_function_by_name)(reflection, c_name.as_ptr());
            if function.is_null() || (f.function_is_overloaded)(function) {
                continue;
            }
            let mark = (f.function_user_attribute_by_name)(function, session, mark_name.as_ptr());
            if mark.is_null() {
                continue;
            }
            let mut arguments = Vec::new();
            for i in 0..(f.user_attribute_argument_count)(mark) {
                let mut size = 0usize;
                let text = (f.user_attribute_argument_string)(mark, i, &mut size);
                if !text.is_null() {
                    let bytes = std::slice::from_raw_parts(text.cast::<u8>(), size);
                    let argument = String::from_utf8_lossy(bytes);
                    arguments.push(argument.trim_matches('"').to_owned());
                }
            }
            let mut parameters = Vec::new();
            for i in 0..(f.function_parameter_count)(function) {
                let parameter = (f.function_parameter)(function, i);
                if parameter.is_null() {
                    continue;
                }
                let direction = if !(f.variable_find_modifier)(parameter, MODIFIER_INOUT).is_null()
                {
                    Direction::InOut
                } else if !(f.variable_find_modifier)(parameter, MODIFIER_OUT).is_null() {
                    Direction::Out
                } else {
                    Direction::In
                };
                parameters.push(Parameter {
                    name: string((f.variable_name)(parameter)),
                    ty: value_type(f, (f.variable_type)(parameter)),
                    direction,
                });
            }
            found.push(MarkedFunction {
                name: name.clone(),
                arguments,
                parameters,
                result: value_type(f, (f.function_result_type)(function)),
            });
        }
    }
    found
}

/// # Safety
///
/// `ty` is null or comes from a live reflection.
unsafe fn value_type(f: &Functions, ty: *mut ReflectionType) -> ValueType {
    if ty.is_null() {
        return ValueType::Named(String::from("void"));
    }
    let scalar = |scalar: u32, n: u8| match scalar {
        SCALAR_FLOAT32 => Some(FieldKind::Float(n)),
        SCALAR_UINT32 => Some(FieldKind::Uint(n)),
        SCALAR_INT32 => Some(FieldKind::Int(n)),
        _ => None,
    };
    // SAFETY: guaranteed by the caller.
    unsafe {
        let name = string((f.type_name)(ty));
        let field = match (f.type_kind)(ty) {
            TYPE_KIND_SCALAR => scalar((f.type_scalar)(ty), 1),
            TYPE_KIND_VECTOR => u8::try_from((f.type_element_count)(ty))
                .ok()
                .filter(|n| (1..=4).contains(n))
                .and_then(|n| scalar((f.type_scalar)(ty), n)),
            TYPE_KIND_STRUCT if name == TEXTURE_REF => Some(FieldKind::Texture),
            _ => None,
        };
        field.map_or(ValueType::Named(name), ValueType::Field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names after marks, in order and once each: arguments, other attributes and
    /// line breaks between; a commented mark, a mark on a struct, a longer attribute name
    /// and a generic declaration's name are not taken.
    #[test]
    fn marked_names_are_the_functions_after_the_marks() {
        let source = r#"
[__AttributeUsage(_AttributeTargets.Function)]
struct NodeAttribute { string title; string category; };

// [Node("Gone")] float4 commented(float4 a) { return a; }
/* [Node] float4 blocked(float4 a) { return a; } */
[Node("Triplanar", "Mapping [3D]")]
float4 triplanar(MaterialContext ctx, float3 position, float sharpness) { return 0; }

[Node] [ForceInline]
float3
tint(float3 color, out float luminance) { luminance = 0; return color; }

[Node] struct NotAFunction { float a; };
[NodeLike] float4 other(float4 a) { return a; }
[Node] float4 tint(float4 again) { return again; }
"#;
        assert_eq!(marked_names(source, "Node"), ["triplanar", "tint"]);
        assert_eq!(marked_names(source, "NodeLike"), ["other"]);
        assert!(marked_names(source, "Missing").is_empty());
    }
}
