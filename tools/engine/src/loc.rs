//! `ofeliaengine loc build` and `loc check` (ADR 0102): a game's `loc/` folder compiled into
//! a strings pack, and checked against the source locale and the game's code.

use engine_assets::strings::write_strings_pack;
use engine_loc::{Compiled, Use, check, compile_sources, find_uses, read_sources};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

pub(crate) struct LocArgs {
    build: bool,
    dir: PathBuf,
    source: String,
    out: Option<PathBuf>,
    pseudo: bool,
    level: i32,
    code: Vec<PathBuf>,
}

pub(crate) fn parse_loc(mut args: impl Iterator<Item = String>) -> Result<LocArgs, String> {
    let build = match args.next().as_deref() {
        Some("build") => true,
        Some("check") => false,
        Some(other) => return Err(format!("unknown loc command `{other}`")),
        None => return Err(String::from("missing `build` or `check`")),
    };
    let mut parsed = LocArgs {
        build,
        dir: PathBuf::new(),
        source: String::from("en"),
        out: None,
        pseudo: false,
        level: 19,
        code: Vec::new(),
    };
    let mut dir = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-o" | "--out" if build => parsed.out = Some(PathBuf::from(value("-o")?)),
            "--source" => parsed.source = value("--source")?,
            "--pseudo" if build => parsed.pseudo = true,
            "--level" if build => {
                let level = value("--level")?;
                parsed.level = level
                    .parse()
                    .ok()
                    .filter(|l| (0..=22).contains(l))
                    .ok_or_else(|| format!("--level {level}: 0 to 22"))?;
            }
            "--code" if !build => parsed.code.push(PathBuf::from(value("--code")?)),
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            path if dir.is_none() => dir = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected `{extra}`")),
        }
    }
    parsed.dir = dir.ok_or("no loc folder given")?;
    if build && parsed.out.is_none() {
        return Err(String::from("no output given (-o <OUT.pack>)"));
    }
    Ok(parsed)
}

pub(crate) fn run_loc(args: &LocArgs) -> Result<ExitCode, String> {
    let start = Instant::now();
    let sources = read_sources(&args.dir).map_err(|e| format!("{}: {e}", args.dir.display()))?;
    let compiled = compile_sources(&sources, &args.source, args.pseudo);
    let mut diagnostics = compiled.diagnostics.clone();
    let mut uses_found = None;
    if !args.build {
        let uses = if args.code.is_empty() {
            None
        } else {
            let (uses, files) = scan(&args.code)?;
            uses_found = Some((uses.len(), files));
            Some(uses)
        };
        diagnostics.extend(check(&compiled, uses.as_deref()));
    }
    for d in &diagnostics {
        eprintln!("{d}");
    }
    let errors = diagnostics.iter().filter(|d| d.error).count();
    let warnings = diagnostics.len() - errors;
    println!(
        "{}: {}; {}, {}, in {:.1} ms",
        args.dir.display(),
        summary(&compiled, &args.source),
        count(errors, "error"),
        count(warnings, "warning"),
        start.elapsed().as_secs_f64() * 1e3
    );
    if let Some((uses, files)) = uses_found {
        println!(
            "{} of messages in {} of code",
            count(uses, "use"),
            count(files, "file")
        );
    }
    if errors > 0 {
        return Ok(ExitCode::from(1));
    }
    if let Some(out) = &args.out {
        let written = write_strings_pack(out, &compiled.tables, &args.source, args.level)
            .map_err(|e| format!("{}: {e}", out.display()))?;
        println!(
            "{}: {} entries, {:.1} KB ({:.1} KB raw)",
            out.display(),
            written.entries,
            written.file_bytes as f64 / 1024.0,
            written.bytes as f64 / 1024.0,
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `n thing` or `n things`.
fn count(n: usize, thing: &str) -> String {
    if n == 1 {
        format!("1 {thing}")
    } else {
        format!("{n} {thing}s")
    }
}

/// "3 locales (en, de, ru) and the pseudo-locales en-XA, ar-XB; 120 messages".
fn summary(compiled: &Compiled, source: &str) -> String {
    let pseudo = ["en-XA", "ar-XB"];
    let (made, real): (Vec<&str>, Vec<&str>) = compiled
        .tables
        .iter()
        .map(|t| t.locale.as_str())
        .partition(|l| pseudo.contains(l));
    let messages = compiled
        .tables
        .iter()
        .find(|t| t.locale == source)
        .map_or(0, engine_loc::Strings::messages);
    let mut s = format!("{} ({})", count(real.len(), "locale"), real.join(", "));
    if !made.is_empty() {
        s.push_str(&format!(" and the pseudo-locales {}", made.join(", ")));
    }
    s.push_str(&format!("; {}", count(messages, "message")));
    s
}

/// The uses of messages in the code under `roots` (`.rs`, `.luau`, `.lua`; `target` and
/// hidden folders skipped), and the number of files read.
fn scan(roots: &[PathBuf]) -> Result<(Vec<Use>, usize), String> {
    let mut files = Vec::new();
    for root in roots {
        walk(root, &mut files).map_err(|e| format!("{}: {e}", root.display()))?;
    }
    files.sort();
    let mut uses = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
        uses.extend(find_uses(&f.display().to_string(), &text));
    }
    Ok((uses, files.len()))
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if dir.is_file() {
        files.push(dir.to_owned());
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_dir() {
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                walk(&path, files)?;
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "rs" | "luau" | "lua"))
        {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<LocArgs, String> {
        parse_loc(line.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn parses_loc_options() {
        let build = parse("build loc -o out.pack --source ru --pseudo --level 3")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(build.build && build.pseudo);
        assert_eq!(
            (build.dir, build.out, build.source.as_str(), build.level),
            (
                PathBuf::from("loc"),
                Some(PathBuf::from("out.pack")),
                "ru",
                3
            )
        );
        let check = parse("check loc --code src --code scripts").unwrap_or_else(|e| panic!("{e}"));
        assert!(!check.build);
        assert_eq!(check.source, "en");
        assert_eq!(check.code, [PathBuf::from("src"), PathBuf::from("scripts")]);
        // A pack needs a path; the check writes none, nor does a build read code.
        assert!(parse("build loc").is_err());
        assert!(parse("check loc -o x.pack").is_err());
        assert!(parse("build loc -o x.pack --code src").is_err());
        assert!(parse("build loc -o x.pack --level 23").is_err());
        assert!(parse("check").is_err());
        assert!(parse("watch loc").is_err());
    }
}
