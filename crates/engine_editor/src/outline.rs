//! A code tab's outline (`docs/research/ux/code-editor.md`, "The code's side panel"): the
//! functions, components and types its language declares, each with its line, read from
//! the text by lexical rules (no checker needed, so it shows at once and for every
//! language): Luau's functions, `component("Name"` declarations and types; Slang's
//! structs, interfaces, enums and functions; a text file's sections and headings.

use crate::highlight::{Language, State, TextKind};
use crate::shell::Token;

/// What an outline row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolKind {
    /// A function or a method.
    Function,
    /// A struct, an interface, an enum, a type.
    Type,
    /// A script's component (`component("Door", …)`).
    Component,
    /// A text file's section or heading.
    Heading,
}

/// A declaration of the text, in its order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symbol {
    /// Its line, from 0.
    pub(crate) line: usize,
    /// Its name as written (`Door.on_tick`, `WaterMaterial.surface`, `[package]`).
    pub(crate) name: String,
    pub(crate) kind: SymbolKind,
    /// How deep it is: a method inside its struct one more than the struct.
    pub(crate) depth: u16,
}

/// The declarations of `lines`, in `language`; what is inside comments and strings left
/// out (by the highlighting's rules).
pub(crate) fn symbols<'a>(language: Language, lines: impl Iterator<Item = &'a str>) -> Vec<Symbol> {
    let mut out = Vec::new();
    let mut state = State::Code;
    let mut runs = Vec::new();
    // Slang's braces open: a struct's members are one deeper.
    let mut depth: u16 = 0;
    for (k, line) in lines.enumerate() {
        runs.clear();
        let starts_inside = state != State::Code;
        let next = language.cut(line, state, &mut runs);
        // A line that starts inside a comment or a string declares nothing there.
        if !starts_inside {
            match language {
                Language::Luau => luau(k, line, &runs, &mut out),
                Language::Slang => slang(k, line, &runs, depth, &mut out),
                Language::Text(kind) => text(k, line, kind, &mut out),
            }
        }
        if language == Language::Slang {
            for (i, c) in line.char_indices() {
                let plain = !runs.iter().any(|&(a, b, t)| {
                    a <= i && i < b && matches!(t, Token::Comment | Token::String)
                });
                match c {
                    '{' if plain => depth = depth.saturating_add(1),
                    '}' if plain => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
        state = next;
    }
    out
}

/// The text of `line` from byte `at` with no comment run starting before it.
fn code_part<'a>(line: &'a str, runs: &[(usize, usize, Token)]) -> &'a str {
    let end = runs
        .iter()
        .find(|&&(_, _, t)| t == Token::Comment)
        .map_or(line.len(), |&(a, _, _)| a);
    &line[..end]
}

fn luau(k: usize, line: &str, runs: &[(usize, usize, Token)], out: &mut Vec<Symbol>) {
    let code = code_part(line, runs);
    let t = code.trim_start();
    let depth = u16::from(line.len() != t.len() && line.starts_with([' ', '\t']));
    let rest = t
        .strip_prefix("local function ")
        .or_else(|| t.strip_prefix("function "))
        .or_else(|| t.strip_prefix("export function "));
    if let Some(rest) = rest {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':'))
            .collect();
        if !name.is_empty() {
            out.push(Symbol {
                line: k,
                name,
                kind: SymbolKind::Function,
                depth,
            });
        }
        return;
    }
    // `local Door = component("Door", {`: the component's own name.
    if let Some(at) = code.find("component(") {
        let after = code[at + "component(".len()..].trim_start();
        if let Some(quoted) = after.strip_prefix('"').or_else(|| after.strip_prefix('\'')) {
            let name: String = quoted
                .chars()
                .take_while(|c| *c != '"' && *c != '\'')
                .collect();
            if !name.is_empty() {
                out.push(Symbol {
                    line: k,
                    name,
                    kind: SymbolKind::Component,
                    depth: 0,
                });
            }
        }
        return;
    }
    let ty = t
        .strip_prefix("export type ")
        .or_else(|| t.strip_prefix("type "));
    if let Some(rest) = ty
        && depth == 0
    {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            out.push(Symbol {
                line: k,
                name,
                kind: SymbolKind::Type,
                depth: 0,
            });
        }
    }
}

fn slang(k: usize, line: &str, runs: &[(usize, usize, Token)], depth: u16, out: &mut Vec<Symbol>) {
    let code = code_part(line, runs);
    let word = |s: &str| -> String {
        s.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    };
    let t = code.trim_start();
    for (key, kind) in [
        ("struct ", SymbolKind::Type),
        ("interface ", SymbolKind::Type),
        ("enum ", SymbolKind::Type),
        ("extension ", SymbolKind::Type),
        ("namespace ", SymbolKind::Type),
    ] {
        if let Some(rest) = t.strip_prefix(key) {
            let rest = rest.trim_start();
            let name = word(rest);
            // `struct Foo;` declares it elsewhere.
            if !name.is_empty() && rest[name.len()..].trim() != ";" {
                out.push(Symbol {
                    line: k,
                    name,
                    kind,
                    depth,
                });
            }
            return;
        }
    }
    // A function declared at the file's top or in a struct: a type, its name, `(`, and no
    // `;` after (a call or a prototype is not one); the keywords that look alike left out.
    if depth > 1 {
        return;
    }
    let Some(open) = t.find('(') else {
        return;
    };
    let head = t[..open].trim_end();
    let mut words = head.split_whitespace().rev();
    let (Some(name), Some(before)) = (words.next(), words.next()) else {
        return;
    };
    let is_name = |s: &str| {
        !s.is_empty()
            && s.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_alphanumeric() || c == '_')
    };
    const NOT: [&str; 8] = [
        "return", "if", "while", "for", "switch", "else", "new", "case",
    ];
    if !is_name(name) || NOT.contains(&name) || NOT.contains(&before) || t.trim_end().ends_with(';')
    {
        return;
    }
    if !(is_name(before.trim_end_matches(['>', '*', '&'])) || before.ends_with('>')) {
        return;
    }
    out.push(Symbol {
        line: k,
        name: name.to_owned(),
        kind: SymbolKind::Function,
        depth,
    });
}

fn text(k: usize, line: &str, kind: TextKind, out: &mut Vec<Symbol>) {
    let t = line.trim();
    let heading = match kind {
        // `[package]`, `[[bin]]`.
        TextKind::Toml => (t.starts_with('[') && t.ends_with(']')).then(|| (t.to_owned(), 0)),
        // `## Rooms`: its level.
        TextKind::Markdown => {
            let level = t.chars().take_while(|&c| c == '#').count();
            (level > 0 && t[level..].starts_with(' '))
                .then(|| (t[level..].trim().to_owned(), (level - 1) as u16))
        }
        // A message's id at the line's start: `door-open = …`.
        TextKind::Fluent => {
            let id: String = line
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                .collect();
            (!id.is_empty() && line[id.len()..].trim_start().starts_with('=')).then_some((id, 0))
        }
        TextKind::Ron | TextKind::Json | TextKind::Plain => None,
    };
    if let Some((name, depth)) = heading
        && !name.is_empty()
    {
        out.push(Symbol {
            line: k,
            name,
            kind: SymbolKind::Heading,
            depth,
        });
    }
}

/// The symbol whose place holds line `line`: the last declared at or before it.
pub(crate) fn current(symbols: &[Symbol], line: usize) -> Option<usize> {
    symbols.iter().rposition(|s| s.line <= line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(language: Language, text: &str) -> Vec<(String, SymbolKind, u16)> {
        symbols(language, text.lines())
            .into_iter()
            .map(|s| (s.name, s.kind, s.depth))
            .collect()
    }

    #[test]
    fn luau_declares_functions_components_and_types() {
        let text = "--!strict\nlocal Door = component(\"Door\", { open = false })\n\nfunction Door.on_tick(self, dt)\n    local function helper() end\nend\n--[[\nfunction Not.this()\n]]\nexport type Key = string\nlocal x = \"function nope()\"\n";
        assert_eq!(
            names(Language::Luau, text),
            [
                (String::from("Door"), SymbolKind::Component, 0),
                (String::from("Door.on_tick"), SymbolKind::Function, 0),
                (String::from("helper"), SymbolKind::Function, 1),
                (String::from("Key"), SymbolKind::Type, 0),
            ]
        );
    }

    #[test]
    fn slang_declares_types_and_functions_not_calls() {
        let text = "struct WaterParams { float4 tint; };\nstruct WaterMaterial : IMaterial\n{\n    static Surface surface(MaterialContext ctx, WaterParams p, bool masked)\n    {\n        Surface s = surface_terms(s);\n        if (masked) return s;\n        return s;\n    }\n}\n// float4 commented(float2 uv)\nfloat4 shade(float2 uv)\n{\n    return tint(uv);\n}\n";
        assert_eq!(
            names(Language::Slang, text),
            [
                (String::from("WaterParams"), SymbolKind::Type, 0),
                (String::from("WaterMaterial"), SymbolKind::Type, 0),
                (String::from("surface"), SymbolKind::Function, 1),
                (String::from("shade"), SymbolKind::Function, 0),
            ]
        );
    }

    #[test]
    fn text_files_list_their_sections_and_headings() {
        assert_eq!(
            names(
                Language::Text(TextKind::Toml),
                "[package]\nname = \"a\"\n[[bin]]\n"
            ),
            [
                (String::from("[package]"), SymbolKind::Heading, 0),
                (String::from("[[bin]]"), SymbolKind::Heading, 0),
            ]
        );
        assert_eq!(
            names(
                Language::Text(TextKind::Markdown),
                "# Title\ntext\n## Part\n#nope\n"
            ),
            [
                (String::from("Title"), SymbolKind::Heading, 0),
                (String::from("Part"), SymbolKind::Heading, 1),
            ]
        );
        assert_eq!(
            names(
                Language::Text(TextKind::Fluent),
                "door-open = The door opens\n    .attr = x\n"
            ),
            [(String::from("door-open"), SymbolKind::Heading, 0)]
        );
    }

    #[test]
    fn the_current_symbol_is_the_last_declared_before_the_line() {
        let s = symbols(
            Language::Luau,
            "local a = 1\nfunction f()\nend\nfunction g()\nend\n".lines(),
        );
        assert_eq!(current(&s, 0), None);
        assert_eq!(current(&s, 2), Some(0));
        assert_eq!(current(&s, 4), Some(1));
    }
}
