//! Slang's highlighting in the code editor (ADR 0107's E10-2a,
//! `docs/research/slang-in-the-editor.md`): a line cut into runs by Slang's lexical rules,
//! as Luau's are ([`crate::highlight`]). What a name is comes from its neighbours: a call's
//! or a declaration's name, a declared type's, the language's keywords and types, `SV_`
//! semantics, and PascalCase names as types (Slang's own convention: `Texture2D`,
//! `IMaterial`, `SamplerState`). What a line starts inside, a block comment or a string
//! going on, comes from the lines before it.

use crate::highlight::{State, next_byte, number, word_end};
use crate::shell::Token;

/// Slang's keywords: declarations, modifiers, statements, and the words of generics,
/// interfaces and automatic differentiation.
const KEYWORDS: &[&str] = &[
    "__exported",
    "__generic",
    "__include",
    "__init",
    "__intrinsic_asm",
    "__ref",
    "__subscript",
    "__target_switch",
    "alignof",
    "as",
    "associatedtype",
    "break",
    "bwd_diff",
    "case",
    "catch",
    "cbuffer",
    "centroid",
    "class",
    "column_major",
    "const",
    "constref",
    "continue",
    "countof",
    "default",
    "defer",
    "discard",
    "do",
    "dyn",
    "else",
    "enum",
    "export",
    "extension",
    "extern",
    "false",
    "for",
    "func",
    "fwd_diff",
    "get",
    "globallycoherent",
    "groupshared",
    "if",
    "implementing",
    "import",
    "in",
    "init",
    "inline",
    "inout",
    "interface",
    "internal",
    "is",
    "let",
    "linear",
    "module",
    "mutating",
    "namespace",
    "new",
    "no_diff",
    "nointerpolation",
    "none",
    "nonmutating",
    "noperspective",
    "nullptr",
    "out",
    "override",
    "packoffset",
    "precise",
    "private",
    "property",
    "public",
    "ref",
    "register",
    "return",
    "row_major",
    "sample",
    "set",
    "sizeof",
    "snorm",
    "some",
    "spirv_asm",
    "static",
    "struct",
    "subscript",
    "switch",
    "tbuffer",
    "throws",
    "true",
    "try",
    "typealias",
    "typedef",
    "typename",
    "uniform",
    "unorm",
    "var",
    "volatile",
    "where",
    "while",
];

/// The keywords a declared type's name follows (`struct Water`).
const DECLARES: &[&str] = &[
    "associatedtype",
    "cbuffer",
    "class",
    "enum",
    "extension",
    "interface",
    "namespace",
    "struct",
    "tbuffer",
    "typealias",
    "typedef",
];

/// The keywords a module's name follows (`import noise`).
const MODULES: &[&str] = &["__include", "implementing", "import", "module"];

/// Slang's types whose names are not PascalCase. Their vector and matrix forms (`float3`,
/// `half4x4`) are [`vector_form`]'s.
const TYPES: &[&str] = &[
    "bool",
    "double",
    "float",
    "float16_t",
    "float32_t",
    "float64_t",
    "half",
    "int",
    "int16_t",
    "int32_t",
    "int64_t",
    "int8_t",
    "intptr_t",
    "matrix",
    "min10float",
    "min12int",
    "min16float",
    "min16int",
    "min16uint",
    "string",
    "uint",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "uint8_t",
    "uintptr_t",
    "vector",
    "void",
];

/// The runs of `line`, which starts in `state`: each a byte range and its kind, pushed onto
/// `out` (plain code left out); and the state the next line starts in.
pub(crate) fn line(line: &str, state: State, out: &mut Vec<(usize, usize, Token)>) -> State {
    let b = line.as_bytes();
    let mut i = 0;
    match state {
        State::Block => match find(b, 0, b"*/") {
            Some(end) => {
                out.push((0, end, Token::Comment));
                i = end;
            }
            None => {
                out.push((0, b.len(), Token::Comment));
                return State::Block;
            }
        },
        State::Quoted(quote) => {
            let (end, goes_on) = quoted(b, 0, quote);
            out.push((0, end, Token::String));
            if goes_on {
                return state;
            }
            i = end;
        }
        _ => {}
    }
    // What came last: a declared type's name follows `struct` and the others, a module's
    // `import`.
    let mut after = After::Nothing;
    while i < b.len() {
        let c = b[i];
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            out.push((i, b.len(), Token::Comment));
            return State::Code;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            match find(b, i + 2, b"*/") {
                Some(end) => {
                    out.push((i, end, Token::Comment));
                    i = end;
                    continue;
                }
                None => {
                    out.push((i, b.len(), Token::Comment));
                    return State::Block;
                }
            }
        }
        if c == b'"' || c == b'\'' {
            let (end, goes_on) = quoted(b, i + 1, c);
            out.push((i, end, Token::String));
            if goes_on {
                return State::Quoted(c);
            }
            i = end;
            after = After::Nothing;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let end = number(b, i);
            out.push((i, end, Token::Number));
            i = end;
            after = After::Nothing;
            continue;
        }
        // A preprocessor directive, first on its line: `#` and its word; `#include`'s
        // `<file>` is a string.
        if c == b'#' && line[..i].trim().is_empty() {
            let mut at = i + 1;
            while matches!(b.get(at), Some(b' ' | b'\t')) {
                at += 1;
            }
            let end = word_end(b, at);
            out.push((i, end, Token::Keyword));
            if &line[at..end] == "include"
                && let Some(open) = (end..b.len()).find(|&k| b[k] != b' ' && b[k] != b'\t')
                && b[open] == b'<'
            {
                let close = find(b, open + 1, b">").unwrap_or(b.len());
                out.push((open, close, Token::String));
                i = close;
            } else {
                i = end;
            }
            after = After::Nothing;
            continue;
        }
        // An attribute at a declaration's start: `[shader("compute")]`, `[[vk::binding(0)]]`.
        if c == b'['
            && let Some((at, end)) = attribute(b, i)
        {
            out.push((at, end, Token::Keyword));
            i = end;
            after = After::Nothing;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let end = word_end(b, i);
            let word = &line[i..end];
            let kind = word_kind(line, i, end, after);
            if let Some(kind) = kind {
                out.push((i, end, kind));
            }
            after = match kind {
                Some(Token::Keyword) if DECLARES.contains(&word) => After::Declares,
                Some(Token::Keyword) if MODULES.contains(&word) => After::Module,
                // `import a.b`: the name after the dots too.
                _ if after == After::Module && next_byte(b, end) == Some(b'.') => After::Module,
                _ => After::Nothing,
            };
            i = end;
            continue;
        }
        if !(c == b'.' && after == After::Module) {
            after = After::Nothing;
        }
        i += line[i..].chars().next().map_or(1, char::len_utf8);
    }
    State::Code
}

/// What came last before a word, for what the word is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Nothing,
    /// `struct`, `interface` and the others: a type's name comes.
    Declares,
    /// `import`, `module`, `implementing`: a module's name (and its dots).
    Module,
}

/// What the word `line[at..end]` is, coming after `after`; `None` for plain code.
fn word_kind(line: &str, at: usize, end: usize, after: After) -> Option<Token> {
    let b = line.as_bytes();
    let word = &line[at..end];
    // A field or a method (`p.tint`, `ctx.sample(`): a name, whatever its word.
    let member = at > 0 && b[at - 1] == b'.' && after != After::Module;
    let calls = next_byte(b, end) == Some(b'(');
    if after == After::Declares || after == After::Module {
        return Some(Token::Type);
    }
    if !member && KEYWORDS.contains(&word) {
        return Some(Token::Keyword);
    }
    if !member && (TYPES.contains(&word) || vector_form(word)) {
        return Some(Token::Type);
    }
    if calls {
        return Some(Token::Function);
    }
    if member {
        return None;
    }
    if word.starts_with("SV_") || word == "this" || word == "This" {
        return Some(Token::Builtin);
    }
    // PascalCase: a type, by Slang's convention (`MATERIAL`, all capitals, is a macro).
    let mut chars = word.chars();
    let pascal = chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && word.chars().any(|c| c.is_ascii_lowercase());
    pascal.then_some(Token::Type)
}

/// Whether `word` is a scalar type's vector or matrix form: `float3`, `int2`, `half4x4`.
fn vector_form(word: &str) -> bool {
    let b = word.as_bytes();
    let size = |c: u8| (b'1'..=b'4').contains(&c);
    let base_len =
        if b.len() >= 3 && b[b.len() - 2] == b'x' && size(b[b.len() - 3]) && size(b[b.len() - 1]) {
            b.len() - 3
        } else if !b.is_empty() && size(b[b.len() - 1]) {
            b.len() - 1
        } else {
            return false;
        };
    let base = &word[..base_len];
    base != "string"
        && base != "void"
        && base != "vector"
        && base != "matrix"
        && TYPES.contains(&base)
}

/// An attribute's name at `at`'s `[` (or `[[`): its byte range, with its `::` parts. Only
/// where an attribute can stand: first on the line, or after `(`, `,`, `]`, `{`, `}` or
/// `;`, so that `a[i]` and `float x[4]` stay indexing.
fn attribute(b: &[u8], at: usize) -> Option<(usize, usize)> {
    let before = b[..at]
        .iter()
        .rev()
        .copied()
        .find(|c| *c != b' ' && *c != b'\t');
    if !matches!(before, None | Some(b'(' | b',' | b']' | b'{' | b'}' | b';')) {
        return None;
    }
    let start = if b.get(at + 1) == Some(&b'[') {
        at + 2
    } else {
        at + 1
    };
    if !b
        .get(start)
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
    {
        return None;
    }
    let mut end = start;
    loop {
        end = word_end(b, end);
        if b[end..].starts_with(b"::")
            && b.get(end + 2)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        {
            end += 2;
        } else {
            break;
        }
    }
    Some((start, end))
}

/// Where `what` ends in `b` from `from` on: past it.
fn find(b: &[u8], from: usize, what: &[u8]) -> Option<usize> {
    (from..=b.len().checked_sub(what.len())?)
        .find(|&k| &b[k..k + what.len()] == what)
        .map(|k| k + what.len())
}

/// A quoted string's end from `from` (after its opening `quote`): past its closing quote,
/// or the line's end; and whether it goes on to the next line (`\` last on the line).
fn quoted(b: &[u8], from: usize, quote: u8) -> (usize, bool) {
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\\' if i + 1 >= b.len() => return (b.len(), true),
            b'\\' => i += 2,
            c if c == quote => return (i + 1, false),
            _ => i += 1,
        }
    }
    (b.len().min(i), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runs of `text`'s lines, as `(text, kind)`, from the start state.
    fn runs(text: &str) -> Vec<(String, Token)> {
        let mut state = State::Code;
        let mut all = Vec::new();
        for l in text.lines() {
            let mut out = Vec::new();
            state = line(l, state, &mut out);
            all.extend(out.into_iter().map(|(a, b, k)| (l[a..b].to_owned(), k)));
        }
        all
    }

    fn kind_of(text: &str, word: &str) -> Option<Token> {
        runs(text)
            .into_iter()
            .find(|(t, _)| t == word)
            .map(|(_, k)| k)
    }

    #[test]
    fn declarations_name_their_types_and_functions() {
        let text = "struct WaterMaterial : IMaterial\n{\n    typealias Params = WaterParams;\n    static float4 shade(MaterialContext ctx, float2 uv) { return ctx.sample(uv); }\n}\n";
        assert_eq!(kind_of(text, "struct"), Some(Token::Keyword));
        assert_eq!(kind_of(text, "WaterMaterial"), Some(Token::Type));
        assert_eq!(kind_of(text, "IMaterial"), Some(Token::Type));
        assert_eq!(kind_of(text, "typealias"), Some(Token::Keyword));
        assert_eq!(kind_of(text, "Params"), Some(Token::Type));
        assert_eq!(kind_of(text, "float4"), Some(Token::Type));
        assert_eq!(kind_of(text, "shade"), Some(Token::Function));
        assert_eq!(kind_of(text, "float2"), Some(Token::Type));
        assert_eq!(kind_of(text, "sample"), Some(Token::Function));
        // A local and a field are plain.
        assert_eq!(kind_of(text, "ctx"), None);
        assert_eq!(kind_of(text, "uv"), None);
        assert_eq!(kind_of("float x = p.roughness;", "roughness"), None);
    }

    #[test]
    fn attributes_semantics_numbers_and_directives() {
        let text = "#include \"common.slang\"\n#include <noise.slang>\n[shader(\"compute\")]\n[numthreads(8, 8, 1)]\nvoid main(uint3 id : SV_DispatchThreadID, [[vk::location(0)]] float3 p)\n{\n    float4 c = a[id.x] * 1.5f + 0x1Fu;\n}\n";
        let all = runs(text);
        assert!(all.contains(&(String::from("#include"), Token::Keyword)));
        assert!(all.contains(&(String::from("\"common.slang\""), Token::String)));
        assert!(all.contains(&(String::from("<noise.slang>"), Token::String)));
        assert_eq!(kind_of(text, "shader"), Some(Token::Keyword));
        assert_eq!(kind_of(text, "\"compute\""), Some(Token::String));
        assert_eq!(kind_of(text, "numthreads"), Some(Token::Keyword));
        assert_eq!(kind_of(text, "vk::location"), Some(Token::Keyword));
        assert_eq!(kind_of(text, "SV_DispatchThreadID"), Some(Token::Builtin));
        assert_eq!(kind_of(text, "uint3"), Some(Token::Type));
        assert_eq!(kind_of(text, "1.5f"), Some(Token::Number));
        assert_eq!(kind_of(text, "0x1Fu"), Some(Token::Number));
        // Indexing is not an attribute.
        assert_eq!(kind_of(text, "id"), None);
        // A macro in capitals is plain; `void` and `main(` are a type and a function.
        assert_eq!(kind_of("MATERIAL::load(ctx);", "MATERIAL"), None);
        assert_eq!(kind_of(text, "void"), Some(Token::Type));
        assert_eq!(kind_of(text, "main"), Some(Token::Function));
    }

    #[test]
    fn a_block_comment_and_a_string_go_on_across_lines() {
        let mut out = Vec::new();
        let state = line("float a; /* one", State::Code, &mut out);
        assert_eq!(state, State::Block);
        out.clear();
        let state = line("two */ return;", state, &mut out);
        assert_eq!(state, State::Code);
        assert_eq!(out[0], (0, 6, Token::Comment));
        assert_eq!(out[1], (7, 13, Token::Keyword));
        out.clear();
        let state = line("// a /* not a block", State::Code, &mut out);
        assert_eq!(state, State::Code);
        let state = line("string s = \"a\\", State::Code, &mut out);
        assert_eq!(state, State::Quoted(b'"'));
        out.clear();
        assert_eq!(line("b\"; x", state, &mut out), State::Code);
        assert_eq!(out[0], (0, 2, Token::String));
    }

    #[test]
    fn imports_name_their_modules() {
        let text = "import engine.material;\nimplementing noise;\n";
        assert_eq!(kind_of(text, "import"), Some(Token::Keyword));
        assert_eq!(kind_of(text, "engine"), Some(Token::Type));
        assert_eq!(kind_of(text, "material"), Some(Token::Type));
        assert_eq!(kind_of(text, "noise"), Some(Token::Type));
    }
}
