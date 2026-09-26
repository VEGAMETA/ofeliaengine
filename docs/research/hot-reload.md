# Hot reload and file watching: research

Date: 2026-09-26. Input for [ADR 0009](../adr/0009-shader-hot-reload.md). We read the code
for ideas only. No code was copied.

## Sources

| Source | Version checked | License | What we looked at |
|---|---|---|---|
| [Bevy](https://github.com/bevyengine/bevy): `crates/bevy_asset/src/io/file/file_watcher.rs`, `io/source.rs` | `main`, fetched 2026-09-26 | MIT / Apache-2.0 | asset hot reload, debouncing |
| [notify](https://github.com/notify-rs/notify): `notify/src/windows.rs` | `main` (9.0.0-rc.5), fetched 2026-09-26 | CC0-1.0 | the `ReadDirectoryChangesW` backend Bevy uses |
| [Godot](https://github.com/godotengine/godot): `editor/file_system/editor_file_system.cpp`, `editor/editor_node.cpp` | `master`, fetched 2026-09-26 | MIT | how the editor notices changed files |
| [Wicked Engine](https://github.com/turanszkij/WickedEngine): `WickedEngine/wiShaderCompiler.cpp` | `master`, fetched 2026-09-26 | MIT | shader dependency tracking and outdated checks |
| Microsoft Learn: `ReadDirectoryChangesW`, `FILE_NOTIFY_INFORMATION` | checked 2026-09-26 | — | buffer limits, overflow, overlapped use |

## How others do it

### Bevy (via notify and notify-debouncer-full)

- The file watcher is `notify` wrapped in `notify-debouncer-full`. The default debounce time
  is **300 ms** (`AssetSource::platform_default` passes `Duration::from_millis(300)` for
  both the source and the processed directories).
- The debouncer merges a rename's "from" and "to" halves and drops duplicate modify events.
  Bevy then maps the events to asset events (added, modified, removed, renamed) and reloads
  the affected assets.
- Pros: portable and battle-tested. Rename handling is correct.
- Cons: every change waits at least 300 ms before anything happens, which is already 60% of
  our 500 ms "shader edit → on screen" budget (CLAUDE.md §5).

### notify on Windows

- `ReadDirectoryChangesW` with **completion routines** (APCs) on a dedicated thread, a
  16 KiB buffer per watch, and a buffer overflow reported as a `Rescan` flag.
- The thread waits with `WaitForSingleObjectEx(semaphore, 100 ms, alertable)`. The
  completion routines run inside that wait. It wakes every 100 ms even when nothing
  happens.
- Pros: one thread for many watches.
- Cons: periodic wake-ups, and the license (CC0-1.0) is not on our allow-list as written
  (§15). The part we need, one recursive watch per shader root on Windows only, is about
  200 lines.

### Godot

- The editor does not use OS notifications for the project directory. It **scans
  modification times** when the application gains focus
  (`NOTIFICATION_APPLICATION_FOCUS_IN` → `EditorFileSystem::scan_changes`). With
  "import resources when unfocused" enabled, it also scans on a 0.5 s timer while
  unfocused.
- Pros: nothing to go wrong with notifications (network drives, overflows). The
  alt-tab-back workflow feels instant enough.
- Cons: a full scan per focus change is proportional to project size. Edits made while the
  editor is focused (a second monitor, an external tool writing files) are noticed late.

### Wicked Engine

- Each compiled shader gets a `.wishadermeta` file listing its dependencies (the includes
  the compiler reported). `IsShaderOutdated` compares the shader's timestamp with each
  dependency's. `CheckRegisteredShadersOutdated` walks all registered shaders, and the
  application polls it.
- Pros: exact dependency tracking from the compiler, the same idea as our cache manifests
  (ADR 0008).
- Cons: timestamps, not content. A touch rebuilds; a checkout that restores an older
  timestamp does not. Polling cost grows with the shader count.

## Measurements (container, 4 vCPU, Slang 2026.12)

Where compile time goes, for a realistic fragment and vertex shader (a PBR loop importing a
shared module):

| Case | Median |
|---|---|
| Trivial compute shader | 8.7 ms |
| Realistic shader, shared module inlined | 60.6 ms |
| Same, importing the module (`import common;`) | 67.3 ms |
| Same, HLSL target instead of SPIR-V (front end + IR only) | 25–34 ms |
| Optimization level 0 / 1 / 2 / 3 | 64 / 66 / 68 / 68 ms |
| Slang session creation | 221 ms, **~150 MB resident per session** |

Conclusions:

- **Per-shader cost dominates**, not the shared module (~7 ms of 67). Caching imported
  modules across compiles (the COM API's `ISession`) would save only about 10%.
- **The optimization level does not matter** at this size. The SPIR-V back end (~35 ms)
  and the front end (~25 ms) are the bulk.
- **Each parallel compile needs its own session**, and each costs ~150 MB (Slang's API is
  single-threaded per session). Parallelism trades memory for time.

## What we take

- **OS notifications, not polling and not a fixed debounce**: an overlapped
  `ReadDirectoryChangesW` per root, waited on together with a stop event. No periodic
  wake-ups; the thread costs nothing while nothing changes.
- **A short "settle" window** (10 ms of quiet, capped at 100 ms) instead of Bevy's 300 ms
  debounce. Editors write a file in bursts (truncate, write, rename). Reading a half-written
  file only produces a transient error that the next event fixes.
- **Compiler-reported dependencies** (as Wicked does), but compared by **content hash**
  (our cache), not timestamps. A touch or a revert with an identical result sends nothing
  to the renderer.
- **Rescan on overflow** (as notify does). Directory renames also trigger a rescan.
- **Stream results as each shader finishes**: after an edit to a module imported by many
  shaders, the first updated shader is usable long before the last one is done.

## What we don't

- A 300 ms debounce, focus-triggered scans and timestamp checks (see above).
- `notify` as a dependency: we are Windows-only, need about 200 lines, and its license needs
  review.
- Module caching through the COM API: ~10% gain, measured above. Revisit when the COM
  migration trigger in ADR 0008 fires.
