//! Finding shader sources on disk and normalizing their paths.

use std::io;
use std::path::{Path, PathBuf};

/// File extension of Slang sources.
pub const SLANG_EXTENSION: &str = "slang";

/// Whether `text` declares at least one entry point (`[shader("...")]`).
///
/// A cheap textual pre-filter: files without entry points are modules, compiled only
/// through the files that import them. Slang itself decides the actual entry points, so
/// a false positive (the attribute inside a comment) costs one compile that finds none.
pub fn declares_entry_points(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find('[') {
        let mut i = from + offset + 1;
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes[i..].starts_with(b"shader") {
            i += "shader".len();
            while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            if bytes.get(i) == Some(&b'(') {
                return true;
            }
        }
        from += offset + 1;
    }
    false
}

/// Whether `path` has the Slang extension.
pub fn is_slang_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(SLANG_EXTENSION))
}

/// Every `.slang` file under `roots` (recursively) that declares entry points, as
/// normalized paths, sorted and without duplicates. Directories whose name starts with
/// `.` are skipped.
pub fn find_shader_sources(roots: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    engine_profiler::zone!("shader::find_shader_sources");
    let mut found = Vec::new();
    let mut stack: Vec<PathBuf> = roots.iter().map(|r| normalize(r)).collect();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !entry.file_name().to_string_lossy().starts_with('.') {
                    stack.push(path);
                }
            } else if is_slang_file(&path)
                && std::fs::read_to_string(&path).is_ok_and(|t| declares_entry_points(&t))
            {
                found.push(normalize(&path));
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// The absolute, canonical form of `path` (symlinks and `..` resolved when the file
/// exists; on Windows without the `\\?\` prefix that `canonicalize` adds).
pub fn normalize(path: &Path) -> PathBuf {
    let absolute = std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_owned());
    strip_verbatim(absolute)
}

/// A key that compares equal for every spelling of the same file: [`normalize`]d and,
/// on Windows, lower-cased (NTFS names are case-insensitive).
pub fn path_key(path: &Path) -> PathBuf {
    let normalized = normalize(path);
    if cfg!(windows) {
        PathBuf::from(normalized.to_string_lossy().to_lowercase())
    } else {
        normalized
    }
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    let stripped = match path.to_string_lossy().strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => Some(PathBuf::from(rest)),
        _ => None,
    };
    stripped.unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_entry_point_attributes() {
        assert!(declares_entry_points("[shader(\"vertex\")]\nV main() {}"));
        assert!(declares_entry_points("[ shader (\"compute\")]"));
        assert!(declares_entry_points(
            "[numthreads(8,8,1)]\n[shader(\"compute\")]"
        ));
        assert!(!declares_entry_points("module common;\nfloat x[4];"));
        assert!(!declares_entry_points("[shaderish(1)] ["));
        assert!(!declares_entry_points(""));
    }

    #[test]
    fn finds_sources_and_skips_modules_and_hidden_dirs() {
        let dir = std::env::temp_dir().join(format!("engine_shader_find_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::create_dir_all(dir.join(".cache")).unwrap();
        let entry = "[shader(\"compute\")] [numthreads(1,1,1)] void main() {}";
        std::fs::write(dir.join("a.slang"), entry).unwrap();
        std::fs::write(dir.join("nested").join("b.SLANG"), entry).unwrap();
        std::fs::write(dir.join("module.slang"), "module m;").unwrap();
        std::fs::write(dir.join("notes.txt"), entry).unwrap();
        std::fs::write(dir.join(".cache").join("c.slang"), entry).unwrap();
        let found = find_shader_sources(&[dir.clone(), dir.join("nested")]).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_lowercase())
            .collect();
        assert_eq!(names, ["a.slang", "b.slang"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
