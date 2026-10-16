//! A game's `loc/` folder (ADR 0102): a subfolder per locale
//! (`loc/en/menu.ftl`, `loc/ru/menu.ftl`), compiled together, and the check of it against
//! the game's code.

use crate::compile::{Diagnostic, Pseudo, compile, compile_pseudo, compile_translation};
use crate::id::hash;
use crate::table::{Op, Strings};
use std::path::{Path, PathBuf};

/// A locale's sources: the `.ftl` files of its subfolder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleSources {
    /// The subfolder's name: the locale's tag (`ru`, `pt-BR`).
    pub locale: String,
    /// Each file's path (the folder as given, joined) and text, sorted by path.
    pub files: Vec<(String, String)>,
}

impl LocaleSources {
    fn borrowed(&self) -> Vec<(&str, &str)> {
        self.files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect()
    }
}

/// The locale subfolders of `dir` that hold `.ftl` files, sorted by locale.
///
/// # Errors
///
/// When `dir` or one of its files cannot be read (a file that is not UTF-8 included).
pub fn read_sources(dir: &Path) -> std::io::Result<Vec<LocaleSources>> {
    let mut locales = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(locale) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let sources = read_locale(dir, &locale)?;
        if !sources.files.is_empty() {
            locales.push(sources);
        }
    }
    locales.sort_by(|a, b| a.locale.cmp(&b.locale));
    Ok(locales)
}

/// The `.ftl` files of `dir/<locale>`, sorted by name.
///
/// # Errors
///
/// When the folder or one of its files cannot be read.
pub fn read_locale(dir: &Path, locale: &str) -> std::io::Result<LocaleSources> {
    let folder = dir.join(locale);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&folder)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ftl")) && p.is_file())
        .collect();
    paths.sort();
    let files = paths
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p)
                .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
            Ok((p.display().to_string(), text))
        })
        .collect::<std::io::Result<_>>()?;
    Ok(LocaleSources {
        locale: locale.to_owned(),
        files,
    })
}

/// A folder's locales compiled.
#[derive(Debug, Clone, Default)]
pub struct Compiled {
    /// The tables: the source locale's first, then the others by locale, then the
    /// pseudo-locales.
    pub tables: Vec<Strings>,
    /// What the compilers found.
    pub diagnostics: Vec<Diagnostic>,
    /// The locales whose files have errors (their tables leave the entries in error out):
    /// a hot reload keeps their previous tables.
    pub broken: Vec<String>,
}

impl Compiled {
    /// Whether a diagnostic is an error.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.error)
    }
}

/// Compiles `locales`: the `source` locale's first ([`compile`]), every other against it
/// ([`compile_translation`]); with `pseudo`, the pseudo-locales `en-XA` and `ar-XB` made
/// from the source's files (unless a folder has their name).
pub fn compile_sources(locales: &[LocaleSources], source: &str, pseudo: bool) -> Compiled {
    let mut out = Compiled::default();
    let Some(sources) = locales.iter().find(|l| l.locale == source) else {
        out.diagnostics.push(Diagnostic {
            file: source.to_owned(),
            line: 0,
            column: 0,
            message: format!(
                "no folder of the source locale `{source}`: its messages are what every other locale falls back to"
            ),
            error: true,
        });
        out.broken.push(source.to_owned());
        return out;
    };
    let add = |out: &mut Compiled, table: Strings, d: Vec<Diagnostic>| {
        if d.iter().any(|d| d.error) {
            out.broken.push(table.locale.clone());
        }
        out.diagnostics.extend(d);
        out.tables.push(table);
    };
    let (table, d) = compile(source, &sources.borrowed());
    add(&mut out, table, d);
    for l in locales.iter().filter(|l| l.locale != source) {
        let (t, d) = compile_translation(&l.locale, &l.borrowed(), &out.tables[0]);
        add(&mut out, t, d);
    }
    if pseudo {
        for p in [Pseudo::Accented, Pseudo::RightToLeft] {
            if locales.iter().any(|l| l.locale == p.locale()) {
                continue;
            }
            // Its diagnostics are the source's again.
            let (t, _) = compile_pseudo(p, &sources.borrowed());
            out.tables.push(t);
        }
    }
    out
}

/// A message used in code: `loc!("id")` or `loc!("id", "attribute")` in Rust, `loc("id")`
/// (or `loc "id"`) in Luau.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Use {
    /// The message's identifier.
    pub id: String,
    /// Its attribute, if one is named.
    pub attribute: Option<String>,
    /// The file, as given.
    pub file: String,
    /// Line, from 1.
    pub line: u32,
    /// Column (characters), from 1.
    pub column: u32,
}

/// The messages `text` uses: Rust's `loc!` in a `.rs` file, Luau's `loc` otherwise. Lines
/// that start a comment are skipped.
pub fn find_uses(file: &str, text: &str) -> Vec<Use> {
    let rust = file.ends_with(".rs");
    let (call, comment) = if rust { ("loc!", "//") } else { ("loc", "--") };
    let mut uses = Vec::new();
    for (k, line) in text.lines().enumerate() {
        if line.trim_start().starts_with(comment) {
            continue;
        }
        let mut from = 0;
        while let Some(at) = line[from..].find(call) {
            let start = from + at;
            from = start + call.len();
            // Not the end of a longer name (`myloc(`).
            if line[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                continue;
            }
            let rest = line[from..].trim_start();
            let rest = match rest.strip_prefix('(') {
                Some(r) => r.trim_start(),
                // Luau calls with a string literal without parentheses.
                None if !rust && rest.starts_with(['"', '\'']) => rest,
                None => continue,
            };
            let Some((id, after)) = literal(rest, rust) else {
                continue;
            };
            let attribute = after
                .trim_start()
                .strip_prefix(',')
                .and_then(|r| literal(r.trim_start(), rust))
                .map(|(a, _)| a.to_owned());
            uses.push(Use {
                id: id.to_owned(),
                attribute,
                file: file.to_owned(),
                line: k as u32 + 1,
                column: line[..start].chars().count() as u32 + 1,
            });
        }
    }
    uses
}

/// A string literal at the start of `s` (`"…"`, or `'…'` in Luau) and what follows it.
fn literal(s: &str, rust: bool) -> Option<(&str, &str)> {
    let quote = s
        .chars()
        .next()
        .filter(|&c| c == '"' || (!rust && c == '\''))?;
    let body = &s[1..];
    let end = body.find(quote)?;
    Some((&body[..end], &body[end + 1..]))
}

/// What a check reports besides the compilers' diagnostics:
///
/// - each locale's messages the source has and it lacks (a warning per locale: they show in
///   the source locale);
/// - with `uses`, messages the code uses that the source does not define (errors: the game
///   shows their identifiers), and the source's messages neither the code nor another
///   message uses (a warning).
pub fn check(compiled: &Compiled, uses: Option<&[Use]>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Some(source) = compiled.tables.first() else {
        return out;
    };
    let pseudo = [Pseudo::Accented.locale(), Pseudo::RightToLeft.locale()];
    let all: Vec<&str> = source.entry_names().collect();
    for t in compiled.tables[1..]
        .iter()
        .filter(|t| !pseudo.contains(&t.locale.as_str()))
    {
        let have: std::collections::HashSet<&str> = t.entry_names().collect();
        let mut missing: Vec<&str> = all.iter().copied().filter(|n| !have.contains(n)).collect();
        missing.sort_unstable();
        if missing.is_empty() {
            continue;
        }
        const SHOWN: usize = 20;
        let mut names: Vec<String> = missing
            .iter()
            .take(SHOWN)
            .map(|n| format!("`{n}`"))
            .collect();
        if missing.len() > SHOWN {
            names.push(format!("and {} more", missing.len() - SHOWN));
        }
        out.push(Diagnostic {
            file: t.locale.clone(),
            line: 0,
            column: 0,
            message: format!(
                "lacks {} of the source's {} messages and attributes (they show in `{}`): {}",
                missing.len(),
                all.len(),
                source.locale,
                names.join(", ")
            ),
            error: false,
        });
    }
    let Some(uses) = uses else {
        return out;
    };
    for u in uses {
        let attribute = u.attribute.as_deref().map_or(0, |a| hash(a.as_bytes()));
        if source
            .find(false, hash(u.id.as_bytes()), attribute)
            .is_none()
        {
            let shown = match &u.attribute {
                Some(a) => format!("{}.{a}", u.id),
                None => u.id.clone(),
            };
            out.push(Diagnostic {
                file: u.file.clone(),
                line: u.line,
                column: u.column,
                message: format!(
                    "`{shown}` is not defined in the source locale `{}`",
                    source.locale
                ),
                error: true,
            });
        }
    }
    // Used: by the code, or referenced by another message.
    let mut used: std::collections::HashSet<u64> =
        uses.iter().map(|u| hash(u.id.as_bytes())).collect();
    for op in &source.ops {
        if let Op::Reference {
            target,
            term: false,
            ..
        } = *op
        {
            used.insert(target);
        }
    }
    let mut unused: Vec<&str> = source
        .message_names()
        .filter(|n| !used.contains(&hash(n.as_bytes())))
        .collect();
    unused.sort_unstable();
    let unused: Vec<String> = unused.iter().map(|n| format!("`{n}`")).collect();
    if !unused.is_empty() {
        out.push(Diagnostic {
            file: source.locale.clone(),
            line: 0,
            column: 0,
            message: format!(
                "{} of {} messages are used neither by the code checked nor by another message: {}",
                unused.len(),
                source.messages(),
                unused.join(", ")
            ),
            error: false,
        });
    }
    out
}
