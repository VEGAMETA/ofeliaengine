//! The compiler (ADR 0102): a locale's Fluent files into a [`Strings`]
//! table, with what is wrong in them.
//!
//! The subset of Fluent compiled: messages and their attributes, terms (with literal
//! arguments and attributes as selectors), variables, message and term references, string
//! and number literals, select expressions (on variables, `NUMBER(...)`, term attributes and
//! literals; keys by CLDR plural category, exact number or name), and `NUMBER` with
//! `minimumFractionDigits`, `maximumFractionDigits`, `useGrouping` and `type: "ordinal"`.
//! `DATETIME` and other functions are reported (a variable is written as it is).
//!
//! A translation compiles against the source locale's table ([`compile_translation`]):
//! references the source resolves are not reported, messages the source lacks are, and so
//! are variables the source's message does not pass (the message is left out: the source's
//! shows instead of a `{$name}` the game never fills in).

use crate::id::hash;
use crate::table::{
    CATEGORIES, Entry, Key, NumberFormat, Op, Selector, Span, Strings, TermArgument, TermValue,
    Variant, variables_across,
};
use fluent_syntax::ast;
use fluent_syntax::parser;
use std::collections::HashMap;

/// Something wrong in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The file's name as given.
    pub file: String,
    /// Line, from 1; 0 for something about the file or locale as a whole.
    pub line: u32,
    /// Column (characters), from 1 (0 with no line).
    pub column: u32,
    /// What is wrong.
    pub message: String,
    /// An error (the table leaves the entry out) rather than a warning.
    pub error: bool,
}

impl core::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = if self.error { "error" } else { "warning" };
        if self.line == 0 {
            return write!(f, "{}: {kind}: {}", self.file, self.message);
        }
        write!(
            f,
            "{}:{}:{}: {kind}: {}",
            self.file, self.line, self.column, self.message
        )
    }
}

/// A pseudo-locale made from the source locale's messages: a testing aid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pseudo {
    /// `en-XA`: letters accented, text a third longer, every message in brackets. Text left
    /// out of localization shows plain; layouts too tight for longer translations overflow.
    Accented,
    /// `ar-XB`: every message forced right to left (U+202E … U+202C).
    RightToLeft,
}

impl Pseudo {
    /// The locale the pseudo-locale is known by.
    pub fn locale(self) -> &'static str {
        match self {
            Self::Accented => "en-XA",
            Self::RightToLeft => "ar-XB",
        }
    }

    fn text(self, text: &str, out: &mut String) {
        match self {
            Self::Accented => {
                for c in text.chars() {
                    let accented = match c {
                        'a' => "á",
                        'e' => "é",
                        'i' => "í",
                        'o' => "ó",
                        'u' => "ú",
                        'y' => "ý",
                        'A' => "Å",
                        'E' => "É",
                        'I' => "Î",
                        'O' => "Ö",
                        'U' => "Û",
                        'c' => "ç",
                        'n' => "ñ",
                        's' => "š",
                        'z' => "ž",
                        _ => {
                            out.push(c);
                            continue;
                        }
                    };
                    out.push_str(accented);
                    // A third longer: every vowel doubled, about one letter in three.
                    if "aeiouyAEIOU".contains(c) {
                        out.push_str(accented);
                    }
                }
            }
            Self::RightToLeft => out.push_str(text),
        }
    }

    fn open(self) -> &'static str {
        match self {
            Self::Accented => "[",
            Self::RightToLeft => "\u{202E}",
        }
    }

    fn close(self) -> &'static str {
        match self {
            Self::Accented => "]",
            Self::RightToLeft => "\u{202C}",
        }
    }
}

/// Compiles a locale's files (`(name, contents)`) into a table, with what was found wrong;
/// entries in error are left out.
pub fn compile(locale: &str, files: &[(&str, &str)]) -> (Strings, Vec<Diagnostic>) {
    compile_as(locale, files, None, None)
}

/// Compiles a translation's files against `source`, the source locale's table (see the
/// module documentation).
pub fn compile_translation(
    locale: &str,
    files: &[(&str, &str)],
    source: &Strings,
) -> (Strings, Vec<Diagnostic>) {
    compile_as(locale, files, None, Some(source))
}

/// The pseudo-locale `pseudo` of the source locale's files.
pub fn compile_pseudo(pseudo: Pseudo, files: &[(&str, &str)]) -> (Strings, Vec<Diagnostic>) {
    compile_as(pseudo.locale(), files, Some(pseudo), None)
}

fn compile_as(
    locale: &str,
    files: &[(&str, &str)],
    pseudo: Option<Pseudo>,
    source: Option<&Strings>,
) -> (Strings, Vec<Diagnostic>) {
    let mut c = Compiler {
        strings: Strings {
            locale: locale.to_owned(),
            ..Strings::default()
        },
        diagnostics: Vec::new(),
        pseudo,
        source,
        file: "",
        lines: Vec::new(),
        text: "",
        defined: HashMap::new(),
        references: Vec::new(),
    };
    for &(name, contents) in files {
        let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
        c.file = name;
        c.text = contents;
        c.lines = core::iter::once(0)
            .chain(contents.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let resource = match parser::parse(contents) {
            Ok(r) => r,
            Err((r, errors)) => {
                for e in errors {
                    c.report(e.pos.start, format!("{}", e.kind), true);
                }
                r
            }
        };
        for entry in &resource.body {
            c.entry(entry);
        }
    }
    c.finish();
    // In the order of the files and positions.
    c.diagnostics
        .sort_by(|a, b| (&a.file, a.line, a.column).cmp(&(&b.file, b.line, b.column)));
    (c.strings, c.diagnostics)
}

struct Compiler<'s> {
    strings: Strings,
    diagnostics: Vec<Diagnostic>,
    pseudo: Option<Pseudo>,
    /// The source locale's table, for a translation.
    source: Option<&'s Strings>,
    file: &'s str,
    /// The current file's text.
    text: &'s str,
    /// Where each line of the current file starts.
    lines: Vec<usize>,
    /// Entries defined: (term, hash, attribute) → where.
    defined: HashMap<(bool, u64, u64), (String, u32, u32)>,
    /// References made: (from entry, to (term, hash, attribute), where).
    references: Vec<Reference>,
}

/// A reference from one entry to another, and where it is written.
struct Reference {
    /// The referring entry: (term, hash).
    from: (bool, u64),
    /// The referred: (term, hash, attribute).
    to: (bool, u64, u64),
    file: String,
    line: u32,
    column: u32,
    /// As written (`-brand.gender`).
    written: String,
}

impl<'s> Compiler<'s> {
    /// Line and column of byte `at` of the current file.
    fn position(&self, at: usize) -> (u32, u32) {
        let line = self.lines.partition_point(|&s| s <= at).max(1);
        let start = self.lines[line - 1];
        let column = self
            .text
            .get(start..at.min(self.text.len()))
            .map_or(0, |s| s.chars().count());
        (line as u32, column as u32 + 1)
    }

    /// The byte offset of `slice` (a part of the current source) in it.
    fn offset(&self, slice: &str) -> usize {
        let base = self.text.as_ptr() as usize;
        let at = slice.as_ptr() as usize;
        at.checked_sub(base)
            .filter(|&o| o <= self.text.len())
            .unwrap_or(0)
    }

    fn report(&mut self, at: usize, message: String, error: bool) {
        let (line, column) = self.position(at);
        self.diagnostics.push(Diagnostic {
            file: self.file.to_owned(),
            line,
            column,
            message,
            error,
        });
    }

    fn text(&mut self, s: &str) -> Span {
        let start = self.strings.text.len() as u32;
        self.strings.text.push_str(s);
        Span {
            start,
            len: s.len() as u32,
        }
    }

    fn entry(&mut self, entry: &ast::Entry<&'s str>) {
        match entry {
            ast::Entry::Message(m) => {
                if let Some(value) = &m.value {
                    self.define(false, m.id.name, None, value, true);
                }
                for a in &m.attributes {
                    self.define(false, m.id.name, Some(a.id.name), &a.value, true);
                }
            }
            ast::Entry::Term(t) => {
                self.define(true, t.id.name, None, &t.value, false);
                for a in &t.attributes {
                    self.define(true, t.id.name, Some(a.id.name), &a.value, false);
                }
            }
            ast::Entry::Junk { .. } => {}
            _ => {}
        }
    }

    fn define(
        &mut self,
        term: bool,
        id: &'s str,
        attribute: Option<&'s str>,
        pattern: &ast::Pattern<&'s str>,
        bracket: bool,
    ) {
        let h = hash(id.as_bytes());
        let a = attribute.map_or(0, |n| hash(n.as_bytes()));
        let (line, column) = self.position(self.offset(id));
        let shown = match attribute {
            Some(n) => format!("{}{id}.{n}", if term { "-" } else { "" }),
            None => format!("{}{id}", if term { "-" } else { "" }),
        };
        if let Some((file, l, c)) = self.defined.get(&(term, h, a)) {
            let message = format!(
                "`{shown}` is defined again (first at {file}:{l}:{c}); this one is left out"
            );
            self.report(self.offset(id), message, true);
            return;
        }
        self.defined
            .insert((term, h, a), (self.file.to_owned(), line, column));
        let from = (term, h);
        let mut span = self.pattern(pattern, from);
        if let (Some(p), true) = (self.pseudo, bracket && attribute.is_none()) {
            // The value of each message in brackets (or an override): around its operations.
            let open = self.text(p.open());
            let close = self.text(p.close());
            let start = self.strings.ops.len() as u32;
            self.strings.ops.push(Op::Text(open));
            let inner: Vec<Op> = self.strings.ops[span.range()].to_vec();
            self.strings.ops.extend(inner);
            self.strings.ops.push(Op::Text(close));
            span = Span {
                start,
                len: span.len + 2,
            };
        }
        let name = match attribute {
            Some(n) => self.text(&format!("{id}.{n}")),
            None => self.text(id),
        };
        self.strings.entries.push(Entry {
            hash: h,
            attribute: a,
            term,
            pattern: span,
            name,
        });
    }

    /// Compiles a pattern into its own span of operations (variants' patterns first).
    fn pattern(&mut self, pattern: &ast::Pattern<&'s str>, from: (bool, u64)) -> Span {
        let mut ops: Vec<Op> = Vec::with_capacity(pattern.elements.len());
        for element in &pattern.elements {
            match element {
                ast::PatternElement::TextElement { value } => {
                    let mut out = String::new();
                    match self.pseudo {
                        Some(p) => p.text(value, &mut out),
                        None => out.push_str(value),
                    }
                    self.push_text(&mut ops, &out);
                }
                ast::PatternElement::Placeable { expression } => {
                    self.expression(expression, from, &mut ops)
                }
            }
        }
        let start = self.strings.ops.len() as u32;
        let len = ops.len() as u32;
        self.strings.ops.extend(ops);
        Span { start, len }
    }

    /// Literal text, merged into the previous operation when that is text too.
    fn push_text(&mut self, ops: &mut Vec<Op>, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(Op::Text(prev)) = ops.last_mut()
            && (prev.start + prev.len) as usize == self.strings.text.len()
        {
            self.strings.text.push_str(s);
            prev.len += s.len() as u32;
            return;
        }
        let span = self.text(s);
        ops.push(Op::Text(span));
    }

    fn expression(&mut self, e: &ast::Expression<&'s str>, from: (bool, u64), ops: &mut Vec<Op>) {
        match e {
            ast::Expression::Inline(inline) => self.inline(inline, from, ops),
            ast::Expression::Select { selector, variants } => {
                let Some(selector) = self.selector(selector) else {
                    return;
                };
                let mut compiled = Vec::with_capacity(variants.len());
                let mut default = 0;
                for v in variants {
                    let key = match &v.key {
                        ast::VariantKey::Identifier { name } => {
                            match CATEGORIES.iter().position(|c| c == name) {
                                Some(c) => Key::Category(c as u8),
                                None => Key::Name(hash(name.as_bytes())),
                            }
                        }
                        ast::VariantKey::NumberLiteral { value } => match value.parse::<f64>() {
                            Ok(n) => Key::Number(n),
                            Err(_) => {
                                self.report(
                                    self.offset(value),
                                    format!("`{value}` is not a number"),
                                    true,
                                );
                                continue;
                            }
                        },
                    };
                    if v.default {
                        default = compiled.len() as u32;
                    }
                    let pattern = self.pattern(&v.value, from);
                    compiled.push(Variant { key, pattern });
                }
                if compiled.is_empty() {
                    return;
                }
                let start = self.strings.variants.len() as u32;
                let len = compiled.len() as u32;
                self.strings.variants.extend(compiled);
                ops.push(Op::Select {
                    selector,
                    variants: Span { start, len },
                    default,
                });
            }
        }
    }

    fn selector(&mut self, s: &ast::InlineExpression<&'s str>) -> Option<Selector> {
        Some(match s {
            ast::InlineExpression::VariableReference { id } => Selector::Variable {
                name: hash(id.name.as_bytes()),
                written: self.text(id.name),
                format: NumberFormat::default(),
            },
            ast::InlineExpression::FunctionReference { id, arguments } if id.name == "NUMBER" => {
                let (variable, format) = self.number(arguments, id.name)?;
                Selector::Variable {
                    name: hash(variable.as_bytes()),
                    written: self.text(variable),
                    format,
                }
            }
            ast::InlineExpression::TermReference {
                id,
                attribute: Some(attribute),
                ..
            } => Selector::TermAttribute {
                term: hash(id.name.as_bytes()),
                attribute: hash(attribute.name.as_bytes()),
            },
            ast::InlineExpression::StringLiteral { value } => {
                let text = unescape(value);
                Selector::Text(self.text(&text))
            }
            ast::InlineExpression::NumberLiteral { value } => {
                Selector::Number(value.parse().unwrap_or(0.0))
            }
            other => {
                let at = self.inline_offset(other);
                self.report(at, String::from("this cannot select variants: a variable, NUMBER(), a term's attribute or a literal can"), true);
                return None;
            }
        })
    }

    /// `NUMBER($var, options)`: the variable's name and its format.
    fn number(
        &mut self,
        arguments: &ast::CallArguments<&'s str>,
        name: &'s str,
    ) -> Option<(&'s str, NumberFormat)> {
        let Some(ast::InlineExpression::VariableReference { id }) = arguments.positional.first()
        else {
            self.report(
                self.offset(name),
                String::from("NUMBER takes a variable"),
                true,
            );
            return None;
        };
        let mut format = NumberFormat::default();
        for named in &arguments.named {
            let value = match &named.value {
                ast::InlineExpression::NumberLiteral { value }
                | ast::InlineExpression::StringLiteral { value } => *value,
                _ => "",
            };
            match named.name.name {
                "minimumFractionDigits" => {
                    format.min_fraction = value.parse().unwrap_or(0).min(9);
                    format.max_fraction = format.max_fraction.max(format.min_fraction);
                }
                "maximumFractionDigits" => {
                    format.max_fraction = value.parse().unwrap_or(3).min(9);
                    format.min_fraction = format.min_fraction.min(format.max_fraction);
                }
                "useGrouping" => format.grouping = value != "false",
                "type" => format.ordinal = value == "ordinal",
                other => {
                    let message = format!("NUMBER's `{other}` is not supported; ignored");
                    self.report(self.offset(named.name.name), message, false);
                }
            }
        }
        Some((id.name, format))
    }

    fn inline(&mut self, e: &ast::InlineExpression<&'s str>, from: (bool, u64), ops: &mut Vec<Op>) {
        match e {
            ast::InlineExpression::StringLiteral { value } => {
                let text = unescape(value);
                self.push_text(ops, &text);
            }
            ast::InlineExpression::NumberLiteral { value } => self.push_text(ops, value),
            ast::InlineExpression::VariableReference { id } => {
                let written = self.text(id.name);
                ops.push(Op::Variable {
                    name: hash(id.name.as_bytes()),
                    written,
                    format: NumberFormat::default(),
                });
            }
            ast::InlineExpression::FunctionReference { id, arguments } => match id.name {
                "NUMBER" => {
                    if let Some((variable, format)) = self.number(arguments, id.name) {
                        let written = self.text(variable);
                        ops.push(Op::Variable {
                            name: hash(variable.as_bytes()),
                            written,
                            format,
                        });
                    }
                }
                "DATETIME" => {
                    self.report(
                        self.offset(id.name),
                        String::from(
                            "DATETIME is not supported yet: the value is written as it is",
                        ),
                        false,
                    );
                    if let Some(ast::InlineExpression::VariableReference { id }) =
                        arguments.positional.first()
                    {
                        let written = self.text(id.name);
                        ops.push(Op::Variable {
                            name: hash(id.name.as_bytes()),
                            written,
                            format: NumberFormat::default(),
                        });
                    }
                }
                other => {
                    let message = format!("the function `{other}` is not known");
                    self.report(self.offset(id.name), message, true);
                }
            },
            ast::InlineExpression::MessageReference { id, attribute } => {
                let target = hash(id.name.as_bytes());
                let a = attribute.as_ref().map_or(0, |n| hash(n.name.as_bytes()));
                let written = self.text(&match attribute {
                    Some(n) => format!("{}.{}", id.name, n.name),
                    None => id.name.to_owned(),
                });
                let (line, column) = self.position(self.offset(id.name));
                self.references.push(Reference {
                    from,
                    to: (false, target, a),
                    file: self.file.to_owned(),
                    line,
                    column,
                    written: self.strings.text(written).to_owned(),
                });
                ops.push(Op::Reference {
                    target,
                    attribute: a,
                    term: false,
                    arguments: Span::default(),
                    written,
                });
            }
            ast::InlineExpression::TermReference {
                id,
                attribute,
                arguments,
            } => {
                let target = hash(id.name.as_bytes());
                let a = attribute.as_ref().map_or(0, |n| hash(n.name.as_bytes()));
                let mut args = Vec::new();
                if let Some(call) = arguments {
                    if !call.positional.is_empty() {
                        self.report(
                            self.offset(id.name),
                            String::from(
                                "a term takes named arguments only; the others are ignored",
                            ),
                            false,
                        );
                    }
                    for named in &call.named {
                        let value = match &named.value {
                            ast::InlineExpression::StringLiteral { value } => {
                                let text = unescape(value);
                                TermValue::Text(self.text(&text))
                            }
                            ast::InlineExpression::NumberLiteral { value } => {
                                TermValue::Number(value.parse().unwrap_or(0.0))
                            }
                            _ => {
                                self.report(
                                    self.offset(named.name.name),
                                    String::from("a term's arguments are literals"),
                                    true,
                                );
                                continue;
                            }
                        };
                        args.push(TermArgument {
                            name: hash(named.name.name.as_bytes()),
                            value,
                        });
                    }
                }
                let start = self.strings.arguments.len() as u32;
                let len = args.len() as u32;
                self.strings.arguments.extend(args);
                let written = self.text(&match attribute {
                    Some(n) => format!("-{}.{}", id.name, n.name),
                    None => format!("-{}", id.name),
                });
                let (line, column) = self.position(self.offset(id.name));
                self.references.push(Reference {
                    from,
                    to: (true, target, a),
                    file: self.file.to_owned(),
                    line,
                    column,
                    written: self.strings.text(written).to_owned(),
                });
                ops.push(Op::Reference {
                    target,
                    attribute: a,
                    term: true,
                    arguments: Span { start, len },
                    written,
                });
            }
            ast::InlineExpression::Placeable { expression } => {
                self.expression(expression, from, ops)
            }
        }
    }

    /// Where an inline expression starts, for a report.
    fn inline_offset(&self, e: &ast::InlineExpression<&'s str>) -> usize {
        match e {
            ast::InlineExpression::StringLiteral { value }
            | ast::InlineExpression::NumberLiteral { value } => self.offset(value),
            ast::InlineExpression::FunctionReference { id, .. }
            | ast::InlineExpression::MessageReference { id, .. }
            | ast::InlineExpression::TermReference { id, .. }
            | ast::InlineExpression::VariableReference { id } => self.offset(id.name),
            ast::InlineExpression::Placeable { .. } => 0,
        }
    }

    /// Sorts the entries; reports references to nothing (warnings: another locale may have
    /// them) and cycles (errors: the entries in one are left out).
    fn finish(&mut self) {
        self.strings
            .entries
            .sort_by_key(|e| (e.term, e.hash, e.attribute));
        let source = self.source;
        let known = |t: bool, h: u64| {
            self.defined.keys().any(|&(dt, dh, _)| dt == t && dh == h)
                || source.is_some_and(|s| s.entries.iter().any(|e| e.term == t && e.hash == h))
        };
        let mut missing = Vec::new();
        for r in &self.references {
            let (term, h, a) = r.to;
            let in_source = source.is_some_and(|s| s.find(term, h, a).is_some());
            if !self.defined.contains_key(&(term, h, a)) && !in_source {
                let what = if known(term, h) {
                    "has no such attribute"
                } else {
                    "is not defined"
                };
                missing.push(Diagnostic {
                    file: r.file.clone(),
                    line: r.line,
                    column: r.column,
                    message: format!("`{}` {what} in this locale", r.written),
                    error: false,
                });
            }
        }
        self.diagnostics.extend(missing);
        // Cycles: a depth-first walk over entry-to-entry references.
        let mut edges: HashMap<(bool, u64), Vec<(bool, u64)>> = HashMap::new();
        for r in &self.references {
            edges.entry(r.from).or_default().push((r.to.0, r.to.1));
        }
        let mut state: HashMap<(bool, u64), u8> = HashMap::new();
        let mut cyclic = Vec::new();
        let nodes: Vec<(bool, u64)> = edges.keys().copied().collect();
        for start in nodes {
            if state.get(&start).copied().unwrap_or(0) != 0 {
                continue;
            }
            let mut stack = vec![(start, 0usize)];
            state.insert(start, 1);
            while let Some(top) = stack.last_mut() {
                let (node, next) = *top;
                let targets = edges.get(&node).map_or(&[][..], Vec::as_slice);
                if next < targets.len() {
                    let t = targets[next];
                    top.1 += 1;
                    match state.get(&t).copied().unwrap_or(0) {
                        0 => {
                            state.insert(t, 1);
                            stack.push((t, 0));
                        }
                        1 => cyclic.extend(stack.iter().map(|&(n, _)| n).skip_while(|&n| n != t)),
                        _ => {}
                    }
                } else {
                    state.insert(node, 2);
                    stack.pop();
                }
            }
        }
        if !cyclic.is_empty() {
            cyclic.sort_unstable();
            cyclic.dedup();
            let names: Vec<String> = self
                .strings
                .entries
                .iter()
                .filter(|e| e.attribute == 0 && cyclic.contains(&(e.term, e.hash)))
                .map(|e| {
                    format!(
                        "{}{}",
                        if e.term { "-" } else { "" },
                        self.strings.text(e.name)
                    )
                })
                .collect();
            self.diagnostics.push(Diagnostic {
                file: self.file.to_owned(),
                line: 1,
                column: 1,
                message: format!(
                    "references go round in a cycle: {}; left out",
                    names.join(", ")
                ),
                error: true,
            });
            self.strings
                .entries
                .retain(|e| !cyclic.contains(&(e.term, e.hash)));
        }
        if let Some(source) = self.source {
            self.against(source);
        }
    }

    /// A translation against the source's table: its messages the source lacks (warnings),
    /// variables the source's message does not pass (errors; the message is left out).
    fn against(&mut self, source: &Strings) {
        let mut messages: Vec<u64> = self
            .strings
            .entries
            .iter()
            .filter(|e| !e.term)
            .map(|e| e.hash)
            .collect();
        messages.dedup();
        for &h in &messages {
            let Some(name) = self.strings.message_name(h) else {
                continue;
            };
            if !source.has_message(name) {
                let (file, line, column) = self.defined_at(h);
                let message = format!(
                    "`{name}` is not in the source locale `{}`: the game cannot know it",
                    source.locale
                );
                self.diagnostics.push(Diagnostic {
                    file,
                    line,
                    column,
                    message,
                    error: false,
                });
            }
        }
        // Variables: the messages that use some their source message does not pass are left
        // out. A message reaches others' variables through its references, which resolve as
        // the runtime does: in the translation, else (it lacks them, or they are left out) in
        // the source. So first every message wrong through any reference, then back those
        // right once the others are left out: a message right with the source's version of a
        // reference stays right with a translation's that passes (its variables are a subset
        // of the source's), so bringing one back never makes another wrong.
        let candidates: Vec<u64> = messages
            .iter()
            .copied()
            .filter(|&h| {
                self.strings
                    .message_name(h)
                    .is_some_and(|n| source.has_message(n))
            })
            .collect();
        let mut left_out: Vec<u64> = candidates
            .iter()
            .copied()
            .filter(|&h| !self.unpassed(source, h, &[]).is_empty())
            .collect();
        loop {
            let back: Vec<u64> = left_out
                .iter()
                .copied()
                .filter(|&h| self.unpassed(source, h, &left_out).is_empty())
                .collect();
            if back.is_empty() {
                break;
            }
            left_out.retain(|h| !back.contains(h));
        }
        for &h in &left_out {
            let extra: Vec<String> = self
                .unpassed(source, h, &left_out)
                .iter()
                .map(|v| format!("`${v}`"))
                .collect();
            let passed: Vec<String> = variables_across(&[source], 0, h, &|_, _, _| 0)
                .iter()
                .map(|v| format!("`${v}`"))
                .collect();
            let name = self.strings.message_name(h).unwrap_or_default().to_owned();
            let (file, line, column) = self.defined_at(h);
            self.diagnostics.push(Diagnostic {
                file,
                line,
                column,
                message: format!(
                    "`{name}` uses {}, which the source's message does not pass ({}); left out, the source's shows",
                    extra.join(", "),
                    if passed.is_empty() {
                        String::from("it passes none")
                    } else {
                        format!("it passes {}", passed.join(", "))
                    }
                ),
                error: true,
            });
        }
        self.strings
            .entries
            .retain(|e| e.term || !left_out.contains(&e.hash));
    }

    /// Where the message of `h` is defined: its value, else its first attribute.
    fn defined_at(&self, h: u64) -> (String, u32, u32) {
        self.defined
            .iter()
            .filter(|((t, dh, _), _)| !t && *dh == h)
            .map(|((_, _, a), at)| (*a != 0, at.clone()))
            .min()
            .map(|(_, at)| at)
            .unwrap_or_default()
    }

    /// The variables message `h` of the translation uses that its source message does not
    /// pass, with the messages of `left_out` resolved in the source.
    fn unpassed(&self, source: &Strings, h: u64, left_out: &[u64]) -> Vec<String> {
        let table = &self.strings;
        let pick = |from: usize, target: u64, attribute: u64| {
            let here = from == 0
                && !left_out.contains(&target)
                && table.find(false, target, attribute).is_some();
            if here { 0 } else { 1 }
        };
        let passed = variables_across(&[source], 0, h, &|_, _, _| 0);
        variables_across(&[table, source], 0, h, &pick)
            .into_iter()
            .filter(|v| !passed.contains(v))
            .map(str::to_owned)
            .collect()
    }
}

/// A string literal's escapes resolved: `\"`, `\\`, `\uHHHH`, `\UHHHHHH`.
fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
            }
            Some('U') => {
                let hex: String = chars.by_ref().take(6).collect();
                out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}
