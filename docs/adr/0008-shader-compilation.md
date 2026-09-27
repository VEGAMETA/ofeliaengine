# 0008. Shader compilation: in-process Slang with a content-addressed cache

- Status: Accepted
- Date: 2026-09-25
- Research: [docs/research/shader-compilation.md](../research/shader-compilation.md)

## Context

CLAUDE.md §9 asks for "one button" shader compilation:
- Slang → SPIR-V;
- a cache keyed by the hash of source, includes, flags and compiler version;
- a shared DDC;
- hot reload within 500 ms;
- "Compile All Shaders" on all cores.

The bootstrap triangle used SPIR-V compiled by hand.

## Options

1. **`slangc` subprocess per compilation.** Simplest, and there is no ABI risk. But each
   process re-creates a session: 242 ms instead of 14 ms per compile (17×).
2. **In-process through the COM API.** The recommended API, but it needs hand-written vtables
   (error-prone) or a young binding crate that links Slang at build time.
3. **In-process through the flat C API, loaded at run time** (chosen). Fast, trivially bound and
   ABI-stable, but deprecated. The migration trigger is recorded.

## Decision

- **`engine_shader::SlangLibrary`** loads the Slang compiler library at run time. It looks, in
  order, for:
  1. `ENGINE_SLANG_LIBRARY`;
  2. `%VULKAN_SDK%\Bin\slang-compiler.dll`, then `slang.dll`;
  3. the OS search path.

  If nothing is found, the error lists every path tried. The editor and tools need Slang;
  shipped games do not.
- **`Compiler`** owns one session and is `Send` but not `Sync`: one per compiling thread. Its
  output is SPIR-V 1.6 per entry point, the full list of dependencies, and diagnostics
  (file:line:column).
- **`ShaderCache`** is a content-addressed directory:
  - a manifest per request key (compiler version, options, entry points, source path and
    content), listing each dependency's content hash and each output's hash;
  - SPIR-V blobs stored by hash;
  - atomic writes (temporary file, then rename), so it can be shared by processes and machines
    as the DDC;
  - a failed cache write costs only a recompile.
- **`engine_core::hash::StableHasher`**: 128-bit FNV-1a, with a promise never to change,
  because cache file names depend on it.
- The committed bootstrap SPIR-V is checked against the source by a test whenever the
  compiler version matches (2026.12 at first; 2026.18.3 since 2026-09-27, which honors
  `precise` in SPIR-V).
- **Migration trigger for the COM API:** Slang removes the flat API, or we need precompiled
  modules, link-time specialization or program reflection.

## Measurements

- Container: creating a session takes 178–262 ms; compiling the triangle takes 19–27 ms.
- Tests on lavapipe/Linux with Slang 2026.12:
  - the output matches the committed SPIR-V exactly;
  - a module imported with `import common;` appears in the dependencies, and editing it
    invalidates the cache;
  - different options produce a different cache entry;
  - errors come with `file:4:11`;
  - defines change the output.
- The GPU workflow runs the same tests on ref-1 with the Vulkan SDK's Slang (a different
  version, so the drift check is skipped there).

## Consequences

- Hot reload (next): a file watcher (`ReadDirectoryChangesW` in `engine_platform`) maps
  changed files to cache entries through their dependency lists.
- "Compile All Shaders": a job-system scope with one `Compiler` per worker (thread-local,
  created lazily).
- The PSO collection and warm-up (§9) consume the cached SPIR-V.
