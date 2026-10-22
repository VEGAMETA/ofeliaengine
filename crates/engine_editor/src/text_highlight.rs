//! Text files' highlighting in the code editor (`docs/research/ux/code-editor.md`, "The
//! code's side panel"): TOML, RON, JSON, Fluent and Markdown by their lexical rules, so a
//! project's settings, data, localization and notes read as they do in VS Code; plain text
//! as it is. What a line starts inside (a RON block comment, a Markdown fence) comes from
//! the lines before it.

use crate::highlight::{State, TextKind, number, word_end};
use crate::shell::Token;

/// The runs of `line` of a file of `kind`, which starts in `state`: each a byte range and
/// its kind, pushed onto `out` (plain text left out); and the state the next line starts
/// in.
pub(crate) fn line(
    kind: TextKind,
    line: &str,
    state: State,
    out: &mut Vec<(usize, usize, Token)>,
) -> State {
    match kind {
        TextKind::Toml => toml(line, out),
        TextKind::Ron => return ron(line, state, out),
        TextKind::Json => json(line, out),
        TextKind::Fluent => fluent(line, out),
        TextKind::Markdown => return markdown(line, state, out),
        TextKind::Plain => {}
    }
    State::Code
}

/// A quoted string's end from `from` (after its opening `quote`): past its closing quote,
/// or the line's end.
fn quoted(b: &[u8], from: usize, quote: u8) -> usize {
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// The first byte after `at` that is not a space.
fn next_byte(b: &[u8], at: usize) -> Option<u8> {
    b[at.min(b.len())..]
        .iter()
        .copied()
        .find(|c| *c != b' ' && *c != b'\t')
}

fn toml(line: &str, out: &mut Vec<(usize, usize, Token)>) {
    let b = line.as_bytes();
    let t = line.trim_start();
    let start = line.len() - t.len();
    // `[package]`, `[[bin]]`: the whole header.
    if t.starts_with('[') {
        let end = t.find(']').map_or(line.len(), |e| {
            let e = start + e + 1;
            if b.get(e) == Some(&b']') { e + 1 } else { e }
        });
        out.push((start, end, Token::Keyword));
        if let Some(c) = line[end..].find('#') {
            out.push((end + c, line.len(), Token::Comment));
        }
        return;
    }
    let mut i = 0;
    let mut key = true;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' => i += 1,
            b'#' => {
                out.push((i, b.len(), Token::Comment));
                return;
            }
            b'"' | b'\'' => {
                let end = quoted(b, i + 1, c);
                out.push((i, end, if key { Token::Function } else { Token::String }));
                i = end;
            }
            b'=' => {
                key = false;
                i += 1;
            }
            _ if c.is_ascii_digit()
                || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                let end = number(b, i + usize::from(c == b'-'));
                out.push((i, end, Token::Number));
                i = end;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let mut end = word_end(b, i);
                while key && matches!(b.get(end), Some(b'-' | b'.')) {
                    end = word_end(b, end + 1);
                }
                let word = &line[i..end];
                if key {
                    out.push((i, end, Token::Function));
                } else if matches!(word, "true" | "false" | "inf" | "nan") {
                    out.push((i, end, Token::Keyword));
                }
                i = end;
            }
            _ => i += line[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
}

fn ron(line: &str, state: State, out: &mut Vec<(usize, usize, Token)>) -> State {
    let b = line.as_bytes();
    let mut i = 0;
    if state == State::Block {
        match line.find("*/") {
            Some(e) => {
                out.push((0, e + 2, Token::Comment));
                i = e + 2;
            }
            None => {
                out.push((0, b.len(), Token::Comment));
                return State::Block;
            }
        }
    }
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                out.push((i, b.len(), Token::Comment));
                return State::Code;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => match line[i + 2..].find("*/") {
                Some(e) => {
                    out.push((i, i + 2 + e + 2, Token::Comment));
                    i += 2 + e + 2;
                }
                None => {
                    out.push((i, b.len(), Token::Comment));
                    return State::Block;
                }
            },
            b'"' => {
                let end = quoted(b, i + 1, b'"');
                out.push((i, end, Token::String));
                i = end;
            }
            _ if c.is_ascii_digit()
                || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                let end = number(b, i + usize::from(c == b'-'));
                out.push((i, end, Token::Number));
                i = end;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let end = word_end(b, i);
                let word = &line[i..end];
                let kind = match next_byte(b, end) {
                    _ if matches!(word, "true" | "false" | "Some" | "None") => Some(Token::Keyword),
                    // `Material(`: a struct's name; `tint:` a field's.
                    Some(b'(') => Some(Token::Type),
                    Some(b':') if b.get(end + 1) != Some(&b':') => Some(Token::Function),
                    _ if word.starts_with(|c: char| c.is_ascii_uppercase()) => Some(Token::Type),
                    _ => None,
                };
                if let Some(kind) = kind {
                    out.push((i, end, kind));
                }
                i = end;
            }
            _ => i += line[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    State::Code
}

fn json(line: &str, out: &mut Vec<(usize, usize, Token)>) {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' => {
                let end = quoted(b, i + 1, b'"');
                // A key is a string before `:`.
                let kind = if next_byte(b, end) == Some(b':') {
                    Token::Function
                } else {
                    Token::String
                };
                out.push((i, end, kind));
                i = end;
            }
            _ if c.is_ascii_digit()
                || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                let end = number(b, i + usize::from(c == b'-'));
                out.push((i, end, Token::Number));
                i = end;
            }
            _ if c.is_ascii_alphabetic() => {
                let end = word_end(b, i);
                if matches!(&line[i..end], "true" | "false" | "null") {
                    out.push((i, end, Token::Keyword));
                }
                i = end;
            }
            _ => i += line[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
}

fn fluent(line: &str, out: &mut Vec<(usize, usize, Token)>) {
    let b = line.as_bytes();
    if line.starts_with('#') {
        out.push((0, b.len(), Token::Comment));
        return;
    }
    // A message's or a term's id at the line's start, an attribute's after its indent.
    let t = line.trim_start();
    let start = line.len() - t.len();
    let id_end = |from: usize| {
        let mut e = from;
        while e < b.len() && (b[e].is_ascii_alphanumeric() || matches!(b[e], b'-' | b'_')) {
            e += 1;
        }
        e
    };
    let mut i = 0;
    if start == 0 && (t.starts_with(|c: char| c.is_ascii_alphabetic()) || t.starts_with('-')) {
        let end = id_end(usize::from(t.starts_with('-')));
        if next_byte(b, end) == Some(b'=') {
            let kind = if t.starts_with('-') {
                Token::Type
            } else {
                Token::Function
            };
            out.push((0, end, kind));
            i = end;
        }
    } else if t.starts_with('.') {
        let end = id_end(start + 1);
        out.push((start, end, Token::Function));
        i = end;
    }
    // Placeables: `{ $count }`, `{ -brand }`, `{ NUMBER($n) }`.
    while let Some(open) = line[i..].find('{') {
        let open = i + open;
        let close = line[open..].find('}').map_or(b.len(), |c| open + c + 1);
        out.push((open, close, Token::Builtin));
        i = close;
    }
}

fn markdown(line: &str, state: State, out: &mut Vec<(usize, usize, Token)>) -> State {
    let b = line.as_bytes();
    let fence = line.trim_start().starts_with("```");
    // Inside a fenced block: code, to its closing fence.
    if state == State::Block {
        out.push((0, b.len(), Token::String));
        return if fence { State::Code } else { State::Block };
    }
    if fence {
        out.push((0, b.len(), Token::String));
        return State::Block;
    }
    let level = line.chars().take_while(|&c| c == '#').count();
    if level > 0 && line[level..].starts_with(' ') {
        out.push((0, b.len(), Token::Keyword));
        return State::Code;
    }
    // Code spans, and bold text.
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'`' => {
                let end = line[i + 1..].find('`').map_or(b.len(), |e| i + 1 + e + 1);
                out.push((i, end, Token::String));
                i = end;
            }
            b'*' if b.get(i + 1) == Some(&b'*') => {
                let end = line[i + 2..].find("**").map_or(b.len(), |e| i + 2 + e + 2);
                out.push((i, end, Token::Type));
                i = end;
            }
            _ => i += line[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    State::Code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(kind: TextKind, text: &str) -> Vec<(String, Token)> {
        let mut state = State::Code;
        let mut all = Vec::new();
        for l in text.lines() {
            let mut out = Vec::new();
            state = line(kind, l, state, &mut out);
            all.extend(out.into_iter().map(|(a, b, k)| (l[a..b].to_owned(), k)));
        }
        all
    }

    #[test]
    fn toml_sections_keys_values_and_comments() {
        let r = runs(
            TextKind::Toml,
            "[package] # the crate\nname = \"door\"\nedition.workspace = true\nopt-level = 3\n",
        );
        assert!(r.contains(&(String::from("[package]"), Token::Keyword)));
        assert!(r.contains(&(String::from("# the crate"), Token::Comment)));
        assert!(r.contains(&(String::from("name"), Token::Function)));
        assert!(r.contains(&(String::from("\"door\""), Token::String)));
        assert!(r.contains(&(String::from("edition.workspace"), Token::Function)));
        assert!(r.contains(&(String::from("true"), Token::Keyword)));
        assert!(r.contains(&(String::from("opt-level"), Token::Function)));
        assert!(r.contains(&(String::from("3"), Token::Number)));
    }

    #[test]
    fn ron_structs_fields_and_a_block_comment_across_lines() {
        let r = runs(
            TextKind::Ron,
            "/* a\n b */ Material(\n    tint: (1.0, 0.5, 0.2),\n    texture: Some(\"wall.png\"),\n)\n",
        );
        assert!(r.contains(&(String::from("/* a"), Token::Comment)));
        assert!(r.contains(&(String::from(" b */"), Token::Comment)));
        assert!(r.contains(&(String::from("Material"), Token::Type)));
        assert!(r.contains(&(String::from("tint"), Token::Function)));
        assert!(r.contains(&(String::from("Some"), Token::Keyword)));
        assert!(r.contains(&(String::from("0.5"), Token::Number)));
    }

    #[test]
    fn json_fluent_and_markdown() {
        let r = runs(
            TextKind::Json,
            "{ \"speed\": 2.5, \"on\": true, \"name\": \"x\" }",
        );
        assert!(r.contains(&(String::from("\"speed\""), Token::Function)));
        assert!(r.contains(&(String::from("\"x\""), Token::String)));
        assert!(r.contains(&(String::from("true"), Token::Keyword)));
        let r = runs(
            TextKind::Fluent,
            "# Doors\ndoor-open = { $who } opens it\n    .title = Open\n-brand = Ofelia\n",
        );
        assert!(r.contains(&(String::from("# Doors"), Token::Comment)));
        assert!(r.contains(&(String::from("door-open"), Token::Function)));
        assert!(r.contains(&(String::from("{ $who }"), Token::Builtin)));
        assert!(r.contains(&(String::from(".title"), Token::Function)));
        assert!(r.contains(&(String::from("-brand"), Token::Type)));
        let r = runs(
            TextKind::Markdown,
            "# Title\nsome `code` and **bold**\n```rust\nlet x = 1;\n```\nafter\n",
        );
        assert!(r.contains(&(String::from("# Title"), Token::Keyword)));
        assert!(r.contains(&(String::from("`code`"), Token::String)));
        assert!(r.contains(&(String::from("**bold**"), Token::Type)));
        assert!(
            r.contains(&(String::from("let x = 1;"), Token::String)),
            "{r:?}"
        );
        assert!(
            !r.iter().any(|(t, _)| t == "after"),
            "after the fence: plain"
        );
    }
}
