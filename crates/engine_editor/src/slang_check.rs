//! Slang's checker (ADR 0107's E10-2a, `docs/research/slang-in-the-editor.md`): the front
//! end of `engine_shader`'s compiler on the checker's thread, given a tab's text as it is
//! typed, no code generated.
//!
//! A material type (a module declaring `struct … : IMaterial`) compiles inside the
//! engine's `forward.slang` (ADR 0092), which includes it and gives it `IMaterial`,
//! `MaterialContext` and the rest, so it is checked there: its text goes to a copy in
//! `.ofelia/check/` that `forward.slang` includes. Any other module is checked by itself.
//! What is found in the module is placed in it. An error found elsewhere (in
//! `forward.slang`, in a module it imports) goes on its first line, with where it is; a
//! warning found elsewhere is that file's own, not shown.
//!
//! The engine's own files are never material types: `forward.slang` declares the glTF
//! material and includes the others. Completion, hover, signatures and definitions are
//! Slang's language server's (E10-2b, [`crate::slangd`]).

use crate::checker::Analyzer;
use crate::slangd::Slangd;
use engine_luau_analysis::{Completion, Diagnostic, Severity};
use engine_shader::material_types::material_type_options;
use engine_shader::{CompileOptions, Compiler, Finding, SlangLibrary, path_key};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The front end's checks of a project's Slang modules.
pub(crate) struct SlangAnalyzer {
    compiler: Compiler,
    /// The project's folder.
    root: PathBuf,
    /// The engine's shader folder: `forward.slang` and what it includes.
    engine: PathBuf,
    /// Where a material type's text is copied to be checked inside `forward.slang`.
    copies: PathBuf,
    /// The modules' texts, by name: their tabs', unsaved.
    sources: HashMap<String, String>,
    /// The language server beside the library, for what is not a check.
    server: Slangd,
}

impl SlangAnalyzer {
    /// The checks of the project in `root`; `None`, said once in the log, without Slang's
    /// library (the Vulkan SDK's, or `ENGINE_SLANG_LIBRARY`).
    pub(crate) fn new(root: &Path) -> Option<Self> {
        let loaded = SlangLibrary::load().and_then(|library| {
            let path = library.path().to_path_buf();
            Compiler::new(library).map(|compiler| (compiler, path))
        });
        let (compiler, library) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => {
                log::warn!(target: "engine_editor", "Slang modules are not checked: {e}");
                return None;
            }
        };
        let engine = PathBuf::from(engine_render::forward::ENGINE_SHADER_DIR);
        let copies = root.join(crate::session::DIR).join("check");
        Some(Self {
            compiler,
            root: root.to_path_buf(),
            server: Slangd::new(&library, root, &engine, &copies),
            engine,
            copies,
            sources: HashMap::new(),
        })
    }

    /// The material type module `name` holding `text` declares; none in the engine's own
    /// files.
    fn material_of(&self, name: &str, text: &str) -> Option<String> {
        if path_key(&self.root.join(name)).starts_with(path_key(&self.engine)) {
            return None;
        }
        material_type(text)
    }
}

impl Analyzer for SlangAnalyzer {
    fn set_source(&mut self, name: &str, text: &str) {
        self.sources.insert(name.to_owned(), text.to_owned());
    }

    fn forget(&mut self, name: &str) {
        self.sources.remove(name);
    }

    fn check(&mut self, name: &str) -> Vec<Diagnostic> {
        // The language server starts while the front end checks.
        self.server.warm();
        let Some(text) = self.sources.get(name) else {
            return Vec::new();
        };
        let path = self.root.join(name);
        let (checked, found) = match self.material_of(name, text) {
            Some(type_name) => {
                let file = Path::new(name)
                    .file_name()
                    .map_or_else(|| PathBuf::from("module.slang"), PathBuf::from);
                let copy = self.copies.join(file);
                if let Err(e) =
                    std::fs::create_dir_all(&self.copies).and_then(|()| std::fs::write(&copy, text))
                {
                    let message = format!("{}: {e}", copy.display());
                    return vec![first_line(Severity::Error, message)];
                }
                let mut options = material_type_options(&self.engine, &copy, &type_name);
                // What it includes, from its own folder.
                if let Some(dir) = path.parent() {
                    options.include_dirs.push(dir.to_path_buf());
                }
                let forward = self.engine.join("forward.slang");
                (copy, self.compiler.check_file(&forward, &options))
            }
            None => {
                let options = CompileOptions {
                    include_dirs: vec![self.engine.clone(), self.root.clone()],
                    ..CompileOptions::default()
                };
                let found = self.compiler.check_source(&path, text, &options);
                (path, found)
            }
        };
        let module = path_key(&checked);
        found
            .into_iter()
            .filter_map(|f| place(f, &module, text))
            .collect()
    }

    fn complete(&mut self, name: &str, line: u32, byte: u32) -> Vec<Completion> {
        let Some(text) = self.sources.get(name) else {
            return Vec::new();
        };
        let material = self.material_of(name, text).is_some();
        self.server.complete(name, text, material, (line, byte))
    }

    fn hover(&mut self, name: &str, line: u32, byte: u32) -> Option<String> {
        let text = self.sources.get(name)?;
        let material = self.material_of(name, text).is_some();
        self.server.hover(name, text, material, (line, byte))
    }

    fn signature(&mut self, name: &str, line: u32, byte: u32) -> Option<(String, (usize, usize))> {
        let text = self.sources.get(name)?;
        let material = self.material_of(name, text).is_some();
        self.server.signature(name, text, material, (line, byte))
    }

    fn definition(&mut self, name: &str, line: u32, byte: u32) -> Option<(String, u32, u32)> {
        let text = self.sources.get(name)?;
        let material = self.material_of(name, text).is_some();
        self.server.definition(name, text, material, (line, byte))
    }
}

/// The material type `text` declares (`struct Water : IMaterial`), if it is one; its
/// comments left out.
fn material_type(text: &str) -> Option<String> {
    let code = without_comments(text);
    let name_char = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(found) = code[from..].find("struct") {
        let at = from + found;
        from = at + "struct".len();
        let word_alone =
            !code[..at].ends_with(name_char) && code[from..].starts_with(char::is_whitespace);
        if !word_alone {
            continue;
        }
        let rest = code[from..].trim_start();
        let name_len = rest.find(|c: char| !name_char(c)).unwrap_or(rest.len());
        let (name, header) = rest.split_at(name_len);
        let header = &header[..header.find(['{', ';']).unwrap_or(header.len())];
        let implements = header.trim_start().starts_with(':')
            && header
                .split(|c: char| !name_char(c))
                .any(|w| w == "IMaterial");
        if !name.is_empty() && implements {
            return Some(name.to_owned());
        }
    }
    None
}

/// `text` without its comments (`//` to the line's end, `/* */`), each a space.
fn without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.find('\n').map_or("", |n| &after[n..]);
            out.push(' ');
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |n| &after[n + 2..]);
            out.push(' ');
        } else {
            let c = rest.chars().next().unwrap_or(' ');
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// A finding as its tab shows it: in the module `module` (a [`path_key`]) at its place,
/// Slang's column (in characters, from 1) made a byte; an error found elsewhere on the
/// first line with its file and line; a warning found elsewhere not at all.
fn place(f: Finding, module: &Path, text: &str) -> Option<Diagnostic> {
    let severity = if f.error {
        Severity::Error
    } else {
        Severity::Warning
    };
    if path_key(Path::new(&f.path)) != module {
        if !f.error {
            return None;
        }
        let file = Path::new(&f.path)
            .file_name()
            .map_or_else(|| f.path.clone(), |n| n.to_string_lossy().into_owned());
        return Some(first_line(
            severity,
            format!("{file}:{}: {}", f.start.0, f.message),
        ));
    }
    let at = |(line, column): (u32, u32)| {
        let line = line.saturating_sub(1);
        let text = text.split('\n').nth(line as usize).unwrap_or("");
        let byte = text
            .char_indices()
            .nth(column.saturating_sub(1) as usize)
            .map_or(text.len(), |(b, _)| b);
        (line, byte as u32)
    };
    Some(Diagnostic {
        start: at(f.start),
        end: at(f.end),
        severity,
        message: f.message,
    })
}

/// A finding on the module's first line.
fn first_line(severity: Severity, message: String) -> Diagnostic {
    Diagnostic {
        start: (0, 0),
        end: (0, 0),
        severity,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_material_type_is_known_by_its_struct() {
        let text = "// struct Not : IMaterial\n/* struct Nor : IMaterial */\nstruct WaterParams { float4 tint; };\nstruct WaterMaterial\n    : IMaterial\n{\n};\n";
        assert_eq!(material_type(text).as_deref(), Some("WaterMaterial"));
        assert_eq!(material_type("struct P : IFoo { };\nstructure x;\n"), None);
        assert_eq!(
            material_type("float3 noise(float3 p) { return p; }\n"),
            None
        );
    }

    /// The line and byte of `needle` in `text`, `after` bytes into it.
    fn place_of(text: &str, needle: &str, after: usize) -> (u32, u32) {
        let at = text.find(needle).unwrap() + after;
        let line = text[..at].matches('\n').count();
        let start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        (line as u32, (at - start) as u32)
    }

    #[test]
    fn slangd_answers_inside_a_material_type_and_in_a_module_alone() {
        if let Err(e) = SlangLibrary::load() {
            assert!(std::env::var("ENGINE_REQUIRE_SLANG").is_err(), "{e}");
            println!("skipping (no Slang library): {e}");
            return;
        }
        let root = std::env::temp_dir().join(format!("slangd_answers_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("lib")).unwrap();
        let mut slang = SlangAnalyzer::new(&root).unwrap();
        if !slang.server.found() {
            assert!(std::env::var("ENGINE_REQUIRE_SLANG").is_err(), "no slangd");
            println!("skipping (no slangd beside Slang's library)");
            return;
        }
        // The golden shots' material type, read inside `forward.slang`'s prelude.
        let stripes = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/golden/materials/stripes.slang"
        ))
        .unwrap();
        let name = "materials/water.slang";
        slang.set_source(name, &stripes);
        let t = std::time::Instant::now();
        assert_eq!(slang.check(name), []);
        let checked = t.elapsed();
        let t = std::time::Instant::now();
        let items = slang.complete(
            name,
            place_of(&stripes, "ctx.world_position", 4).0,
            place_of(&stripes, "ctx.world_position", 4).1,
        );
        let first = t.elapsed();
        let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
        assert!(
            labels.contains(&"world_position") && labels.contains(&"uv"),
            "{labels:?}"
        );
        // What is at a place: the declaration, written by the server.
        let (line, byte) = place_of(&stripes, "MaterialContext ctx", 3);
        let t = std::time::Instant::now();
        let hover = slang.hover(name, line, byte).unwrap();
        let hovered = t.elapsed();
        assert!(hover.starts_with("struct MaterialContext"), "{hover}");
        // Defined in the prelude: `forward.slang`, at its own line.
        let forward = std::fs::read_to_string(
            Path::new(engine_render::forward::ENGINE_SHADER_DIR).join("forward.slang"),
        )
        .unwrap();
        let (line, byte) = place_of(&stripes, "material_params<", 3);
        let t = std::time::Instant::now();
        let (module, at_line, at_byte) = slang.definition(name, line, byte).unwrap();
        let defined = t.elapsed();
        assert!(module.ends_with("shaders/forward.slang"), "{module}");
        let expected = place_of(&forward, "T material_params<T>(MaterialContext", 2);
        assert_eq!((at_line, at_byte), expected);
        // Defined in the module itself.
        let (line, byte) = place_of(&stripes, "StripesParams load", 3);
        let found = slang.definition(name, line, byte).unwrap();
        assert_eq!(
            found,
            (
                name.to_owned(),
                place_of(&stripes, "StripesParams\n", 0).0,
                7
            )
        );
        // A call's signature, its argument.
        let (line, byte) = place_of(&stripes, "lerp(p.color_a, ", 16);
        let (label, (a, b)) = slang.signature(name, line, byte).unwrap();
        assert!(
            label.contains("lerp") && a < b && b <= label.len(),
            "{label} {a}..{b}"
        );
        // The text changed: what the new line's `s.` may be followed by.
        let edited = stripes.replacen(
            "        Surface s;\n",
            "        Surface s;\n        s.\n",
            1,
        );
        slang.set_source(name, &edited);
        let (line, byte) = place_of(&edited, "        s.\n", 10);
        let t = std::time::Instant::now();
        let items = slang.complete(name, line, byte);
        let again = t.elapsed();
        let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
        assert!(
            labels.contains(&"roughness") && labels.contains(&"base"),
            "{labels:?}"
        );
        // A module alone, on disk: its own fields and definitions.
        let noise = "struct Wave { float height; float speed; };\nfloat crest(Wave w)\n{\n    return w.height;\n}\n";
        std::fs::write(root.join("lib/noise.slang"), noise).unwrap();
        slang.set_source("lib/noise.slang", noise);
        let items = slang.complete("lib/noise.slang", 3, 13);
        let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["height", "speed"]);
        let found = slang.definition("lib/noise.slang", 3, 13).unwrap();
        assert_eq!(found, (String::from("lib/noise.slang"), 0, 20));
        // The language's own (the core module's `frac`): none to go to.
        let (line, byte) = place_of(&stripes, "frac(", 1);
        assert_eq!(slang.definition(name, line, byte), None);
        // `forward.slang` itself is no material type, though it declares one.
        let engine = Path::new(engine_render::forward::ENGINE_SHADER_DIR).join("forward.slang");
        let engine = engine.to_string_lossy().replace('\\', "/");
        assert_eq!(slang.material_of(&engine, &forward), None);
        assert_eq!(
            slang.material_of(name, &stripes).as_deref(),
            Some("StripesMaterial")
        );
        println!(
            "check {:.0} ms (the server starting meanwhile); completion {:.0} ms the first time, {:.0} ms after a change; hover {:.0} ms; definition {:.0} ms",
            checked.as_secs_f64() * 1e3,
            first.as_secs_f64() * 1e3,
            again.as_secs_f64() * 1e3,
            hovered.as_secs_f64() * 1e3,
            defined.as_secs_f64() * 1e3
        );
        drop(slang);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_new_material_types_template_compiles_inside_forward_slang() {
        let dir = std::env::temp_dir().join(format!("slang_template_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(mut analyzer) = SlangAnalyzer::new(&dir) else {
            assert!(
                std::env::var("ENGINE_REQUIRE_SLANG").is_err(),
                "no Slang library"
            );
            return;
        };
        let text = crate::material::type_template("Water");
        analyzer.set_source("water.slang", &text);
        let found = analyzer.check("water.slang");
        let errors: Vec<_> = found
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        // The check is real: a misspelt field is found at its line.
        let typo = text.replacen("p.metallic;", "p.metalic;", 1);
        analyzer.set_source("water.slang", &typo);
        let line = typo.lines().position(|l| l.contains("p.metalic")).unwrap();
        assert!(
            analyzer
                .check("water.slang")
                .iter()
                .any(|d| d.severity == Severity::Error && d.start.0 as usize == line),
            "the typo at line {line}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn findings_are_placed_in_the_module_or_on_its_first_line() {
        let dir = std::env::temp_dir().join(format!("slang_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let module = dir.join("water.slang");
        std::fs::write(&module, "").unwrap();
        let text = "struct P { float a; };\nfloat g(P p) { /* é */ return p.c; }\n";
        let here = Finding {
            path: module.to_string_lossy().replace('\\', "/"),
            start: (2, 33),
            end: (2, 34),
            error: true,
            code: String::from("E30027"),
            message: String::from("'c' is not a member of 'P'."),
        };
        let placed = place(here, &path_key(&module), text).unwrap();
        // Column 33 in characters is byte 33 after the two-byte `é`.
        assert_eq!((placed.start, placed.end), ((1, 33), (1, 34)));
        assert_eq!(placed.severity, Severity::Error);
        let elsewhere = |error: bool| Finding {
            path: String::from("C:/engine/shaders/forward.slang"),
            start: (1990, 5),
            end: (1990, 9),
            error,
            code: String::from("E1"),
            message: String::from("something"),
        };
        let placed = place(elsewhere(true), &path_key(&module), text).unwrap();
        assert_eq!(placed.start, (0, 0));
        assert_eq!(placed.message, "forward.slang:1990: something");
        assert_eq!(placed.severity, Severity::Error);
        // A warning of another file is its own.
        assert_eq!(place(elsewhere(false), &path_key(&module), text), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
