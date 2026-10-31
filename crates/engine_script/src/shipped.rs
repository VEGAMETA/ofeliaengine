//! Scripts as a built game ships them (ADR 0126's G5c, the team's 3а): Luau bytecode
//! instead of source, with what the module requires ahead of it, so the player orders the
//! modules as it does from source without reading it.
//!
//! ```text
//! "OFLUAUBC"  u32 bytes  the requires, as `require("path")` lines  bytecode
//! ```
//!
//! [`compile_shipped`] makes it (the build); [`split_shipped`] reads it (the player), which
//! loads the bytecode with [`crate::ScriptVm::load_module_bytecode`].

use crate::ScriptSettings;

/// The first bytes of a shipped script.
pub const SHIPPED_MAGIC: &[u8; 8] = b"OFLUAUBC";

/// The paths `source` requires by string (`require("./wallet")`, `require "./wallet"`), in
/// order, comments and other strings skipped.
pub fn requires(source: &str) -> Vec<String> {
    let b = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // A long bracket's level at `at` (`[[` 0, `[==[` 2), if one opens there.
    let long = |at: usize| -> Option<(usize, usize)> {
        if b.get(at) != Some(&b'[') {
            return None;
        }
        let mut j = at + 1;
        while b.get(j) == Some(&b'=') {
            j += 1;
        }
        (b.get(j) == Some(&b'[')).then_some((j - at - 1, j + 1))
    };
    let close = |from: usize, level: usize| -> usize {
        let mut j = from;
        while j < b.len() {
            if b[j] == b']' {
                let mut k = j + 1;
                let mut n = 0;
                while b.get(k) == Some(&b'=') {
                    k += 1;
                    n += 1;
                }
                if n == level && b.get(k) == Some(&b']') {
                    return k + 1;
                }
            }
            j += 1;
        }
        b.len()
    };
    let quoted = |from: usize, q: u8| -> (usize, String) {
        let mut j = from;
        let mut text = Vec::new();
        while j < b.len() && b[j] != q && b[j] != b'\n' {
            if b[j] == b'\\' {
                j += 1;
            }
            if let Some(&c) = b.get(j) {
                text.push(c);
            }
            j += 1;
        }
        (j + 1, String::from_utf8_lossy(&text).into_owned())
    };
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                if let Some((level, start)) = long(i + 2) {
                    i = close(start, level);
                } else {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
            }
            b'"' | b'\'' | b'`' => i = quoted(i + 1, b[i]).0,
            b'[' if long(i).is_some() => {
                let (level, start) = long(i).unwrap_or((0, i + 1));
                i = close(start, level);
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let field = start > 0 && matches!(b[start - 1], b'.' | b':');
                if &b[start..i] != b"require" || field {
                    continue;
                }
                let mut j = i;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j) == Some(&b'(') {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                }
                if let Some(&q) = b.get(j)
                    && (q == b'"' || q == b'\'')
                {
                    let (end, path) = quoted(j + 1, q);
                    out.push(path);
                    i = end;
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// `source` compiled as a built game ships it, with `settings`' optimization and debug
/// levels (line numbers kept for errors).
///
/// # Errors
///
/// Its compile error, with its line.
pub fn compile_shipped(source: &str, settings: &ScriptSettings) -> Result<Vec<u8>, String> {
    let bytecode = mlua::chunk::Compiler::new()
        .set_optimization_level(settings.optimization_level)
        .set_debug_level(settings.debug_level)
        .set_type_info_level(1)
        .compile(source)
        .map_err(|e| e.to_string())?;
    let mut needs = String::new();
    for path in requires(source) {
        needs.push_str("require(\"");
        needs.push_str(&path.replace('\\', "\\\\").replace('"', "\\\""));
        needs.push_str("\")\n");
    }
    let mut out = Vec::with_capacity(12 + needs.len() + bytecode.len());
    out.extend_from_slice(SHIPPED_MAGIC);
    out.extend_from_slice(&(needs.len() as u32).to_le_bytes());
    out.extend_from_slice(needs.as_bytes());
    out.extend_from_slice(&bytecode);
    Ok(out)
}

/// A shipped script's requires (as source lines [`requires`] reads) and its bytecode;
/// `None` for anything else (source).
pub fn split_shipped(bytes: &[u8]) -> Option<(&str, &[u8])> {
    let rest = bytes.strip_prefix(SHIPPED_MAGIC)?;
    let length = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    let needs = std::str::from_utf8(rest.get(4..4 + length)?).ok()?;
    Some((needs, rest.get(4 + length..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shipped_script_keeps_its_requires_and_runs() {
        let source = "local function wallet() return require(\"./wallet\") end\n-- require(\"no\")\nreturn { coins = 3, wallet = wallet }\n";
        let shipped = compile_shipped(source, &ScriptSettings::default()).unwrap();
        let (needs, code) = split_shipped(&shipped).unwrap();
        assert_eq!(requires(needs), ["./wallet"]);
        assert!(split_shipped(source.as_bytes()).is_none());
        let mut vm = crate::ScriptVm::new(ScriptSettings::default()).unwrap();
        vm.load_module_bytecode("bank", code).unwrap();
        assert!(vm.has_module("bank"));
        let e = compile_shipped("local x = = 1", &ScriptSettings::default()).unwrap_err();
        assert!(e.contains('1'), "{e}");
    }
}
