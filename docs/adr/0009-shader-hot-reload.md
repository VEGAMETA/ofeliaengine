# 0009. Shader hot reload and "Compile All Shaders"

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/hot-reload.md](../research/hot-reload.md)
- Builds on: [ADR 0008](0008-shader-compilation.md)

## Context

CLAUDE.md §9 asks for:
- hot reload: file save → recompile the affected variants → swap pipelines, with the
  previous version kept on errors;
- a "Compile All Shaders" button in the editor and `engine shaders build` in the CLI:
  - parallel on all cores;
  - cached;
  - a report of variant count, time and errors with file and line.

§5 sets the budget: shader edit → on screen ≤ 500 ms. §12: nothing blocks the UI.

ADR 0008 gave us an in-process compiler and a content-addressed cache. What was missing:
- noticing changes;
- knowing which shaders a changed file affects;
- running compiles without blocking;
- deciding which files are shaders.

## Options

### Detecting changes

1. **Polling modification times** (Godot on focus-in, Wicked). Simple and robust. But the
   cost is proportional to project size, and changes are noticed late.
2. **`notify` crate + 300 ms debounce** (Bevy). Portable, but the debounce alone eats 60%
   of the budget, the thread wakes every 100 ms, and the license (CC0-1.0) needs review.
3. **Own `ReadDirectoryChangesW` watcher** (chosen). Overlapped I/O plus a stop event on one
   thread per root: no wake-ups while idle, a 64 KiB buffer, and a rescan on overflow.
   About 200 lines, Windows-only by design (§1).

### Deciding what to rebuild

1. **Timestamps against a dependency list** (Wicked). A touch rebuilds; reverted files are
   missed.
2. **Compiler-reported dependencies + content hashes** (chosen). Slang reports every file a
   compile read. The cache (ADR 0008) already keys on content. A changed file marks the
   shaders whose dependency lists contain it. A rebuild whose SPIR-V is identical sends
   nothing to the renderer.

### What is a shader

1. **A manifest listing shaders and entry points.** Explicit, but one more thing to keep in
   sync by hand.
2. **Every `.slang` file with a `[shader("...")]` attribute** (chosen). A cheap text check
   selects the files, then Slang discovers the entry points itself (reflection, no entry
   points passed). Files without the attribute are modules, compiled through their
   importers.

### Parallelism and memory

A Slang session is single-threaded, takes ~220 ms to create and holds ~150 MB. We chose:
- **a pool of sessions created on demand**: at most `max_parallel` at once, and never for
  cache hits;
- **dynamic claiming of files** by a fixed number of jobs on the engine job system, so each
  job holds at most one session.

The CLI defaults to `min(logical CPUs, 8)` (~1.2 GB peak). Hot reload keeps **one warm
session** between rebuilds (~150 MB in the editor), so the first edit does not pay for
creating one.

## Decision

- **`engine_platform::fs_watch::DirectoryWatcher`**:
  - one thread per root;
  - overlapped `ReadDirectoryChangesW`, waited on together with a stop event;
  - events delivered through a channel or a caller-supplied sink;
  - `Rescan` on overflow.
- **`engine_shader`**:
  - `find_shader_sources` finds the shaders (see "What is a shader").
  - `CompilerPool` implements the session pool.
  - `ShaderCache::compile` borrows a session only on a miss.
  - `compile_all` / `compile_all_streaming` is "Compile All Shaders". It returns a
    `BuildReport` (files, entry points, compiled / cached / failed, errors with
    file:line:column). The streaming variant also reports each file as it finishes.
  - `ShaderSet` is the platform-independent bookkeeping:
    - tracked sources, their dependency keys and the last good output;
    - `changed(path)` marks exactly the affected shaders;
    - new files, deletions and directory changes are handled by a rescan;
    - failed shaders are retried when any `.slang` file changes, because a missing import
      may have appeared.
  - `ShaderHotReload` runs a `ShaderSet` on a background thread fed by the watchers:
    - it builds everything on start;
    - on each burst of events (settle: 10 ms of quiet, at most 100 ms) it rebuilds;
    - it streams `ShaderEvent`s (`Compiled` / `Failed` / `Removed`) through a channel that
      the render thread polls once per frame with `try_next` (never blocks);
    - on `Failed`, the previous output stays valid.
- **`tools/engine`** (the `engine` binary):
  - `engine shaders build <roots>`: report and exit code (0 success, 1 shader errors,
    2 usage/setup);
  - `engine shaders watch <roots>`: prints rebuilds as they happen.

## Measurements

Container (4 vCPU, **not reference**), Slang 2026.12, release. The corpus is 48 generated
PBR-style shaders (vertex + fragment) importing one module. Hot reload rows use
`notify_changed` (no OS watcher on Linux).

| Case | Result |
|---|---|
| Compile one shader, warm session | 66 ms median |
| Compile All, cold, 1 / 2 / 4 in parallel (sessions included) | 3563 / 1989 / 1548 ms (1.00 / 1.79 / 2.30×) |
| Compile All, all cache hits | 1.1 ms for 48 files |
| Hot reload: edit one shader → new SPIR-V | 77 ms median, 90 ms p90 |
| Hot reload: edit the shared module → **first** of 48 ready | 131 ms median (streaming) |
| Hot reload: edit the shared module → all 48 ready | 1370 ms |

On ref-1 the GPU workflow runs the same benchmark with the real watcher and posts it in its
report. The numbers go into `docs/perf/history.md`.

## Consequences

- **The budget holds for single-shader edits** (77 ms in the container, before pipeline
  creation).
- **A module imported by many shaders exceeds it for the last shader.** The render thread
  gets shaders as they finish, and the first arrives in ~130 ms. Next step when a renderer
  exists: compile shaders in use first (a priority hint from the pipeline registry). More
  parallel sessions help only at the cost of ~150 MB each.
- Pipelines are swapped by the renderer (not yet written). Hot reload delivers SPIR-V per
  source file and entry point.
- Permutations (defines per material) will arrive with the material system. `ShaderSet`
  compiles each source with one option set for now.
- The DDC is the cache directory. Sharing it across a team needs only a shared path;
  eviction is not implemented yet.
