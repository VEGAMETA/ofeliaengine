//! Content-addressed shader cache (CLAUDE.md §9: keyed by source + includes + flags +
//! compiler version).
//!
//! A compilation's dependencies are only known after compiling, so the cache is two-level:
//!
//! 1. The **request key** hashes the compiler version, options, entry points, the source
//!    path and its content. It names a manifest file.
//! 2. The **manifest** lists every dependency with the hash of its content at compile time
//!    and the hash of each entry point's SPIR-V. A lookup re-hashes the dependencies; if
//!    all match, the SPIR-V blobs are loaded by hash.
//!
//! SPIR-V blobs are stored by their own hash, so identical outputs are stored once. Every
//! file is written to a temporary name and renamed, so a crash never leaves a torn entry,
//! and several processes (the team's DDC) can share one directory.

use crate::ShaderError;
use crate::compiler::{CompileOptions, CompileOutput, CompiledEntry, EntryPoint, Stage};
use crate::pool::CompilerPool;
use engine_core::hash::{StableHasher, hash128, to_hex};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Whether a request was served from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// Loaded from the cache without compiling.
    Hit,
    /// Compiled and stored.
    Miss,
}

/// A shader cache directory.
#[derive(Debug, Clone)]
pub struct ShaderCache {
    dir: PathBuf,
}

/// Bumped whenever the compiler's settings change the output (2: column-major matrices).
const MANIFEST_VERSION: &str = "engine-shader-cache 2";

impl ShaderCache {
    /// Opens (creates) a cache in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn request_key(
        compiler_version: &str,
        path: &Path,
        source: &[u8],
        entries: &[EntryPoint],
        options: &CompileOptions,
    ) -> u128 {
        let mut h = StableHasher::new();
        h.write_str(MANIFEST_VERSION);
        h.write_str(compiler_version);
        h.write_str(&path.to_string_lossy());
        h.write(&hash128(source).to_le_bytes());
        h.write_u64(entries.len() as u64);
        for e in entries {
            h.write_str(&e.name);
            h.write_str(e.stage.name());
        }
        h.write_u64(u64::from(options.optimize));
        h.write_u64(u64::from(options.debug_info));
        h.write_u64(options.defines.len() as u64);
        for (k, v) in &options.defines {
            h.write_str(k);
            h.write_str(v);
        }
        h.write_u64(options.include_dirs.len() as u64);
        for d in &options.include_dirs {
            h.write_str(&d.to_string_lossy());
        }
        h.finish()
    }

    fn write_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let final_path = self.dir.join(name);
        let temp = self.dir.join(format!("{name}.tmp{}", std::process::id()));
        fs::write(&temp, bytes)?;
        fs::rename(&temp, &final_path)
    }

    /// Returns the cached output for this request, if every dependency is unchanged.
    fn lookup(&self, key: u128, entries: &[EntryPoint]) -> Option<CompileOutput> {
        let manifest =
            fs::read_to_string(self.dir.join(format!("{}.manifest", to_hex(key)))).ok()?;
        let mut lines = manifest.lines();
        if lines.next()? != MANIFEST_VERSION {
            return None;
        }
        let mut dependencies = Vec::new();
        let mut compiled = Vec::new();
        for line in lines {
            let mut parts = line.splitn(3, '\t');
            match (parts.next()?, parts.next()?, parts.next()?) {
                ("dep", hash, path) => {
                    let current = fs::read(path).ok()?;
                    if to_hex(hash128(&current)) != hash {
                        return None;
                    }
                    dependencies.push(PathBuf::from(path));
                }
                ("entry", hash, name_stage) => {
                    let (name, stage) = name_stage.rsplit_once('\t').unwrap_or((name_stage, ""));
                    let bytes = fs::read(self.dir.join(format!("{hash}.spv"))).ok()?;
                    if !bytes.len().is_multiple_of(4) || to_hex(hash128(&bytes)) != hash {
                        return None;
                    }
                    let spirv = bytes
                        .chunks_exact(4)
                        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    compiled.push(CompiledEntry {
                        name: name.to_owned(),
                        stage: Stage::from_name(stage)?,
                        spirv,
                    });
                }
                _ => return None,
            }
        }
        // No requested entries means "all discovered", which the key already encodes.
        let same_entries = entries.is_empty()
            || (compiled.len() == entries.len()
                && compiled
                    .iter()
                    .zip(entries)
                    .all(|(c, e)| c.name == e.name && c.stage == e.stage));
        same_entries.then_some(CompileOutput {
            entries: compiled,
            dependencies,
            diagnostics: String::new(),
        })
    }

    fn store(&self, key: u128, output: &CompileOutput) -> io::Result<()> {
        let mut manifest = String::from(MANIFEST_VERSION);
        manifest.push('\n');
        for dep in &output.dependencies {
            let bytes = fs::read(dep)?;
            manifest += &format!("dep\t{}\t{}\n", to_hex(hash128(&bytes)), dep.display());
        }
        for entry in &output.entries {
            let bytes: Vec<u8> = entry.spirv.iter().flat_map(|w| w.to_le_bytes()).collect();
            let hash = to_hex(hash128(&bytes));
            let blob = format!("{hash}.spv");
            if !self.dir.join(&blob).exists() {
                self.write_atomic(&blob, &bytes)?;
            }
            manifest += &format!("entry\t{hash}\t{}\t{}\n", entry.name, entry.stage.name());
        }
        self.write_atomic(&format!("{}.manifest", to_hex(key)), manifest.as_bytes())
    }

    /// Compiles `path` through the cache. A hit needs no Slang session; a miss borrows
    /// one from `compilers`. Empty `entries` compiles every `[shader(...)]` function.
    pub fn compile(
        &self,
        compilers: &CompilerPool,
        path: &Path,
        entries: &[EntryPoint],
        options: &CompileOptions,
    ) -> Result<(CompileOutput, CacheOutcome), ShaderError> {
        engine_profiler::zone!("shader::ShaderCache::compile");
        let source = fs::read(path)?;
        let key = Self::request_key(
            compilers.library().version(),
            path,
            &source,
            entries,
            options,
        );
        if let Some(output) = self.lookup(key, entries) {
            return Ok((output, CacheOutcome::Hit));
        }
        let text = String::from_utf8_lossy(&source);
        let output =
            compilers.with(|compiler| compiler.compile_source(path, &text, entries, options))??;
        // A cache write failure costs a recompile next time; it must not fail the build.
        let _ = self.store(key, &output);
        Ok((output, CacheOutcome::Miss))
    }
}
