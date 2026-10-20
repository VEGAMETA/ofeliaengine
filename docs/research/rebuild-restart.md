# Rebuild and restart after a code change: research

Checked 2026-10-20. Input for phase E1-5c of [ADR 0107](../adr/0107-editor.md) §1, "A code
change means a rebuild and a restart", and for the hot patching after it (CLAUDE.md §10).
The editor runs inside the game's executable, so Windows keeps that file locked and a second
`cargo build` cannot replace it. Dioxus, Fyrox and Godot (MIT or Apache-2.0) and LLVM
(Apache-2.0 with LLVM exception) were read as code, for ideas; bacon (AGPL-3.0) only through
its documentation; Unreal and Unity as products. No code was carried over.

## Sources

| Tool | Version, checked | Link | License |
|---|---|---|---|
| Windows: executable images, delete dispositions, `MoveFileExW`, hard links; renaming loaded images (eryksun, go-update #14) | docs and posts, 2026-10-20 | [images](https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/executable-images), [disposition](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntddk/ns-ntddk-_file_disposition_information_ex), [MoveFileExW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw), [FindFirstFileNameW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirstfilenamew), [eryksun](https://bugs.python.org/msg260355), [go-update](https://github.com/inconshreveable/go-update/issues/14) | docs |
| MSVC: LNK1104, LNK1201, `/PDBPATH`; "Improved linker fundamentals in VS 2019" | docs 2026-10-20; post 2019-07-25 | [LNK1104](https://learn.microsoft.com/en-us/cpp/error-messages/tool-errors/linker-tools-error-lnk1104), [LNK1201](https://learn.microsoft.com/en-us/cpp/error-messages/tool-errors/linker-tools-error-lnk1201), [/PDBPATH](https://learn.microsoft.com/en-us/cpp/build/reference/pdbpath), [post](https://devblogs.microsoft.com/cppblog/improved-linker-fundamentals-in-visual-studio-2019/) | docs |
| LLVM lld (COFF): output buffer, Windows rename, `/time` | `main`, 2026-10-20 | [FileOutputBuffer](https://llvm.org/docs/doxygen/FileOutputBuffer_8cpp_source.html), [Path.inc](https://github.com/llvm/llvm-project/blob/main/llvm/lib/Support/Windows/Path.inc), [Options.td](https://github.com/llvm/llvm-project/blob/main/lld/COFF/Options.td) | Apache-2.0 WITH LLVM-exception |
| rustc: `/PDBALTPATH:%_PDB%` (PR #121297, 1.78), JSON diagnostics, codegen options, `share_generics`; Cranelift README and 2025H2 goal | 2026-10-20 | [PR](https://github.com/rust-lang/rust/pull/121297), [JSON](https://doc.rust-lang.org/rustc/json.html), [options](https://doc.rust-lang.org/rustc/codegen-options/index.html), [source](https://doc.rust-lang.org/nightly/nightly-rustc/src/rustc_session/config.rs.html), [Cranelift](https://github.com/rust-lang/rustc_codegen_cranelift), [goal](https://goals.rust-lang.org/2025h2/production-ready-cranelift.html) | MIT / Apache-2.0 |
| Cargo: JSON messages, `cargo build`, profiles, build cache, build performance, unstable, `link_or_copy`; rust-analyzer FAQ | docs for 1.94, 2026-10-20 | [messages](https://doc.rust-lang.org/cargo/reference/external-tools.html), [build](https://doc.rust-lang.org/cargo/commands/cargo-build.html), [profiles](https://doc.rust-lang.org/cargo/reference/profiles.html), [cache](https://doc.rust-lang.org/cargo/reference/build-cache.html), [performance](https://doc.rust-lang.org/cargo/guide/build-performance.html), [unstable](https://doc.rust-lang.org/cargo/reference/unstable.html), [link_or_copy](https://docs.rs/cargo-util/latest/cargo_util/paths/fn.link_or_copy.html), [r-a FAQ](https://rust-analyzer.github.io/book/faq.html) | MIT / Apache-2.0 |
| Rust blog: parallel front end; compiler performance survey | 2023-11-09; 2025-09-10 | [parallel](https://blog.rust-lang.org/2023/11/09/parallel-rustc/), [survey](https://blog.rust-lang.org/2025/09/10/rust-compiler-performance-survey-2025-results) | — |
| cargo-watch | 8.5.3 (2024-10-02), archived | [repo](https://github.com/watchexec/cargo-watch) | CC0-1.0 |
| bacon | 3.26.0 | [config](https://dystroy.org/bacon/config/), [default jobs](https://github.com/Canop/bacon/blob/main/defaults/default-bacon.toml) | AGPL-3.0, docs only |
| Dioxus `dx serve`, Subsecond | 0.7.10 (0.8.0-alpha.3 out) | [subsecond](https://docs.rs/subsecond/0.7.10/subsecond/), [guide](https://dioxuslabs.com/learn/0.7/essentials/ui/hotreload), [builder.rs](https://github.com/DioxusLabs/dioxus/blob/main/packages/cli/src/build/builder.rs), [request.rs](https://github.com/DioxusLabs/dioxus/blob/main/packages/cli/src/build/request.rs), [runner.rs](https://github.com/DioxusLabs/dioxus/blob/main/packages/cli/src/serve/runner.rs) | MIT / Apache-2.0 |
| Bevy: hot patching (0.17), `bevy_hotpatching_experiments`, fast compiles | 0.17 notes 2025-09-30; crate 0.4.0 (2026-01-29) | [0.17](https://bevy.org/news/bevy-0-17/), [crate](https://docs.rs/crate/bevy_hotpatching_experiments/latest), [setup](https://bevy.org/learn/quick-start/getting-started/setup/), [config](https://github.com/bevyengine/bevy/blob/main/.cargo/config_fast_builds.toml) | MIT / Apache-2.0 |
| Fyrox: code hot reloading; hot-lib-reloader | Fyrox 0.34 post, `master`; hot-lib-reloader 0.8.2 | [post](https://fyrox.rs/blog/post/fyrox-game-engine-0-34/), [dylib.rs](https://github.com/FyroxEngine/Fyrox/blob/master/fyrox-impl/src/plugin/dylib.rs), [hot-lib-reloader](https://docs.rs/hot-lib-reloader/0.8.2/hot_lib_reloader/) | MIT |
| Unreal: Live Coding; build refused while active; numbered Hot Reload DLLs | UE 5.8 docs; forums | [docs](https://dev.epicgames.com/documentation/en-us/unreal-engine/using-live-coding-to-recompile-unreal-engine-applications-at-runtime), [forum](https://forums.unrealengine.com/t/ue5-2-unable-to-compile-with-live-coding/1281969), [answers](https://answers.unrealengine.com/questions/659981/view.html) | proprietary (product) |
| Unity: domain reload, Play mode options, reload serialization, iteration time | Unity 6 docs (6000.0, .3, .7); blog 2019-11-05; staff post 2021-10-18 | [domain](https://docs.unity3d.com/6000.0/Documentation/Manual/domain-reloading.html), [play mode](https://docs.unity.com/en-us/engine/6000.7/manual/programming-environment/code-reload-serialization/code-reload/code-reloading-editor/configurable-enter-play-mode), [serialization](https://docs.unity.com/en-us/engine/6000.3/manual/programming-environment/code-reload-serialization/script-serialization/how-unity-uses), [blog](https://unity.com/blog/engine-platform/enter-play-mode-faster-in-unity-2019-3), [post](https://discussions.unity.com/t/improving-iteration-time-on-c-script-changes/859030) | proprietary (product) |
| Godot: C# basics; `csharp_script.cpp`, `GodotPlugins/Main.cs` | 4.7 docs; `master` | [docs](https://docs.godotengine.org/en/stable/tutorials/scripting/c_sharp/c_sharp_basics.html), [csharp_script.cpp](https://github.com/godotengine/godot/blob/master/modules/mono/csharp_script.cpp), [Main.cs](https://github.com/godotengine/godot/blob/master/modules/mono/glue/GodotSharp/GodotPlugins/Main.cs) | MIT |
| Windows: Dev Drive, Defender performance mode and analyzer, block cloning; `DWMWA_CLOAK`, `WINDOWPLACEMENT`, `SetForegroundWindow`, jobs, `CreateProcessW`, firewall rules | docs 2026-10-20; VS blog 2023-05-24 | [Dev Drive](https://learn.microsoft.com/en-us/windows/dev-drive/), [mode](https://learn.microsoft.com/en-us/defender-endpoint/microsoft-defender-endpoint-antivirus-performance-mode), [analyzer](https://learn.microsoft.com/en-us/defender-endpoint/tune-performance-defender-antivirus), [cloning](https://learn.microsoft.com/en-us/windows-server/storage/refs/block-cloning), [blog](https://devblogs.microsoft.com/visualstudio/devdrive), [DWM](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/ne-dwmapi-dwmwindowattribute), [placement](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-windowplacement), [foreground](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow), [jobs](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_associate_completion_port), [flags](https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags), [CreateProcessW](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw), [firewall](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/rules) | docs |
| Ours | 2026-10-20 | `docs/perf/history.md`, `docs/research/pipeline-cache.md`, ADR 0001, ADR 0077, `docs/research/editor-architecture.md`, ref-1's build folder | — |

## Windows specifics

- **A running exe is an image section.** The file system refuses to open it for writing or
  for delete-on-close, "even though the file is not open" (executable images); a delete
  returns `STATUS_CANNOT_DELETE` for "an existing mapped view".
- **A rename works:** the loader opens the file with delete sharing (eryksun; go-update
  hides its running exe this way). Same volume only: across volumes a move is a copy plus a
  delete, and the source stays (`MoveFileExW`).
- **Cargo's two names:** the uplift is a hard link, and "if the destination already exists,
  it is removed before linking" (`link_or_copy`). On MSVC a local binary has no hash in its
  name: ref-1 has `release/deps/editor.exe` and `release/editor.exe`, PDBs beside them.
- **So the next link fails:** link.exe gives LNK1104 ("your program is already running or is
  loaded in the debugger"); lld renames a temporary file over the output, and to move an
  output in use aside opens it with `FILE_FLAG_DELETE_ON_CLOSE` (Path.inc), which an image
  section refuses.
  The player's `cargo run --release` writes the same files and fails the same way.
- **The PDB** is not mapped. Only a debugger or in-process DbgHelp (Tracy's call stacks,
  panic backtraces) locks it: LNK1201 lists "the debugger is active". Since Rust 1.78 the
  exe records only the PDB's file name (PR #121297), and debuggers look "where the
  executable resides" first (`/PDBPATH`): a PDB copied beside a copy is found.

**Strategies:**
- **(a) Run a copy** (Dioxus runs `<stem>-<uuid>.exe` beside the build; Fyrox copies its
  plugin per process). On paper, 20–60 MB from the page cache, where the linker left it: our
  staging bench reads 128 MiB at 5.1 GB/s (history, 2026-09-30), so 4–12 ms to read and as
  much to write; the NVMe (rated 4.4 GB/s) flushes it later, the HDD (about 150 MB/s) in
  0.13–0.4 s; a Dev Drive on 24H2 block-clones it. The unknown is Defender: "Opening a file
  initiates a real-time protection scan", and antivirus "often temporarily block access to
  newly created files, especially .exe" (LNK1104).
- **(b) Rename the running exe aside first.** No copy, the new exe at cargo's path. Both
  hard links move (`FindFirstFileNameW`); a failed build leaves no exe until they come back;
  a debugger still holds the PDB; a crash leaves files to sweep.
- **(c) Another output folder:** `--artifact-dir` is nightly, and its copy fails the same way
  while that copy runs; a second target is a second engine build (74 s); `build-dir` holds
  only intermediates.
- **(d) `--message-format=json`**, one object per line as cargo runs: `compiler-message`
  (rustc's `level`, `message`, `spans` with `file_name`, 1-based `line_start` and
  `column_start`, `is_primary`; `children`), `compiler-artifact` (`executable`; `fresh`:
  rustc did not run), `build-finished` (`success`). Link errors carry the linker's text only.

**Pitfalls of a copy:**
- **Driver shader caches** (`pipeline-cache.md`): a renamed exe starts cold on NVIDIA, a
  moved one on AMD. With our cache file a renamed NVIDIA copy takes 49 ms instead of about
  300 and a moved AMD copy 12 ms, but AMD finds nothing in it for a renamed one.
- **Firewall:** Windows asks the first time a program listens, and rules take "the full path"
  only: a new path per restart is a prompt per restart once Play hosts a session.
- **Files beside the exe, the working folder, the console:** Streamline's DLLs
  (`plugins/nvidia/` next to the exe, `engine_dlss`); `PlayerSettings::project = None` is the
  working folder; `cargo run` returns when its child exits, so a second editor outlives it.

## How others rebuild and relaunch

- **cargo-watch** ("on life support", points to bacon or watchexec) runs cargo on change
  (`check` by default, `-x run`) and restarts a running command unless `--no-restart`, after
  0.5 s. **bacon**'s `run-long` job: `on_change_strategy = "kill_then_restart"`, a default
  watch of `Cargo.toml`, `src`, `tests`, `examples`, `build.rs`, `benches`, output read as
  text or JSON. Both: `cargo run` builds, the old process is killed first, no state.
- **Dioxus `dx serve`, Subsecond:**
  - Calls inside `subsecond::call` go through a jump table to each function's newest
    version. dx captures rustc's arguments (`RUSTC_WORKSPACE_WRAPPER`) and is the linker; a
    "fat" first build (`-Clink-dead-code`, `-Csave-temps`) lets a patch, a dynamic library
    sent to the app, link against every function. State stays. Only the "tip" crate; no
    struct changes; static initializers not re-run; thread-locals reset; only with
    `debug_assertions`; Windows x86-64 works; no timings published.
  - A full rebuild (a deep `Cargo.toml` change, no patch cache): `taskkill` without `/F`, up
    to 1 s, then the new copy, made "so we can run two instances of the same app without
    causing collisions on the filesystem". State is lost.
- **Bevy:** 0.17's `hotpatching` feature, on dx, changes systems while the World stays;
  binary crate only; changed system parameters are not reloaded.
  `bevy_hotpatching_experiments` 0.4.0: changed component structs break queries or are UB;
  on Windows "move the crate closer to the drive root or set codegen-units = 1"; debuggers
  probably do not work.
- **Fyrox:** the game is a plugin DLL (`-C prefer-dynamic=yes`, one std), copied before
  loading because "usually OS locks the library", reloaded at the end of a tick when the
  built DLL changes, plugin entities serialized (`Visit`) and restored around the swap.
- **Unreal Live Coding:** Ctrl+Alt+F11 builds while the editor runs and patches its
  binaries; Object Reinstancing replaces reflected instances after structural changes,
  without it they "will usually result in crashes". A normal build is refused meanwhile
  ("Unable to build while Live Coding is active"); the older Hot Reload built new numbered
  DLLs, since loaded ones cannot be overwritten.
- **Unity:** compiles on an asset refresh and reloads the scripting domain in place (fields
  saved and restored, statics lost) while the editor waits. A one-script change took 2 s,
  later 4–6 s on the Core 3D template, against a goal of 1–2 s (Unity staff, 2021). Unity 6.7
  no longer reloads on entering Play, ahead of CoreCLR, which has no domain reload.
- **Godot (C#):** the editor builds (Build button); the game, its own process, starts again.
  The editor's copy reloads a collectible `AssemblyLoadContext`, properties saved and
  restored around it (unloading gives up after 1 s); the 4.7 docs: state is not restored
  "with the exception of exported variables".
- **Common thread:** reloads keep state by serializing around the swap or by keeping the
  process. Restarting tools kill first; none keeps the old process until the new one is up.

## Build speed knobs

After an edit only the game crate rebuilds: cargo's checks, rustc's front end with the
engine's metadata, codegen of the game and of the engine generics instantiated in it, and
the link of the whole exe.

| Knob | Evidence | For us |
|---|---|---|
| rust-lld or link.exe | ours: 12 % faster on 45 test binaries, one big example relinked in 2.2–3.6 s either way (ADR 0001); Microsoft: link.exe on par with LLD without GHASH | rust-lld stays; `-C link-arg=/time` prints its phases |
| Debug info | ours: full to line tables 84 → 56 s, PDBs 22.5 → 7.8 MB (ADR 0077); survey: 2–30 % fewer cycles; Cargo book: dev `"line-tables-only"`, dependencies `debug = false`; `split-debuginfo` is only `packed` on MSVC | `new`'s dev keeps full debug info for the whole engine: every relink writes a big PDB |
| opt-level, incremental, codegen-units | `new`'s dev: game at 1, dependencies at 3, incremental, 256 units; release: 3, not incremental, 16 | O1 builds faster, runs game code and its generics at O1 in Play; incremental matters once the crate grows |
| `-Zshare-generics` | on by default at opt-level 0, 1, s, z, off at 2 and 3 (rustc source): dependencies at 3 share nothing; Bevy forces it | nightly |
| `-Zthreads=8` | up to 50 %, front end only; dev gains more than release | nightly; a small crate has little front end |
| Cranelift | Windows x86-64 builds, unwinding "not supported on Windows" (`-Cpanic=abort`); about 20 % less codegen, 5 % total (the 2025H2 goal, scoped to Linux and macOS); Bevy: "about 30% faster" | nightly, and no `catch_unwind` at Play's hooks (ADR 0107 §1) |
| Engine as one dylib | Bevy: "the most impactful compilation time decrease"; Windows: "too many exported symbols" without optimizations | the `linkme` type list is per image: two lists |
| Thin generic boundary | `Player::frame_overlaid(&mut impl Game)` and `engine_editor::run::<G>` compile in the game crate, at its opt-level, every rebuild | `&mut dyn Game` inside |
| Dev Drive for `target/` | asynchronous Defender scans, block cloning; about 25 % on dev operations (Microsoft, 2023) | C: cannot be one; ref-1's second NVMe can |

**What likely dominates** (measured below):
- Release (2.5 s, no PDB): rustc on the game crate, mostly O3 codegen of the engine generics
  it instantiates, and metadata loading; linking 19 MB without a PDB is likely under 1 s.
- Dev as `new` writes it: the link, merging the whole engine's debug info into one PDB.
  Linking "is always performed from scratch" (survey), and Bevy's experience agrees. Dev may
  rebuild slower than release unless the dependencies lose their debug info.
- Subsecond needs `debug_assertions`: a dev-like profile is also the road to hot patching.

## The restart

**Numbers we have** (ref-1): the old editor's set-up 0.39 s to its first frame; the editor on
the player, start and 60 frames 0.51–0.69 s; instance and device 154–230 ms, the Radeon's
swapchain 175–181 ms; a demo's first frame 0.26–0.42 s, caches warm; our exe started as the
crash monitor in 10–20 ms; 10,000 entities loaded in 9.65 ms. On paper: copy 10–25 ms, start
0.2–0.35 s, scenes and session tens of ms, two frames: 0.3–0.5 s, plus Defender's scan.

**Spawn early, snapshot late:**
1. `build-finished` with `success` (t0); nothing to do if the game is `fresh` or unchanged.
2. Copy the exe to a free slot, `.ofelia/run/<a|b|c>/<game>.exe`, and spawn it with
   `--handover <name>` (the game's folder, same environment and console, old PID, t0).
3. The new process makes its window cloaked (`DWMWA_CLOAK`: "not visible to the user", still
   "composed by DWM") at the old `WINDOWPLACEMENT` (workspace coordinates, not
   `SetWindowPos`), then device, swapchain, pipelines and fonts, and asks for the session.
4. At a quiet moment (no drag, no text field) the old editor stops taking input, mutes,
   writes and flushes the autosaves and the session (E1-5b), and answers.
5. The new one loads scenes and session, draws its first frames cloaked, reports ready.
6. The old calls `AllowSetForegroundWindow` for it; the new uncloaks over the old window and
   takes the foreground if the old had it; the old hides, releases E1-5b's
   `.ofelia/session.lock` and exits. Only then does the new one write to `.ofelia/`.
7. After step 2 the PDB is copied into the slot under the name the exe records, off the
   critical path: debuggers, Tracy and crash reports read the slot's copy.

The editor is frozen in steps 4–6 only: tens of milliseconds for small scenes. For that
moment two devices, windows and audio streams exist; ADR 0076's budget sees the other one.
- **Crash at start:** the old editor waits on the new process's handle and messages, a
  timeout per step. If the new one exits or stalls, the old takes input again and shows the
  exit code and the new one's stderr in the task panel; the session is untouched.
- **Single instance:** the session lock guards `.ofelia/`; a second `ofeliaengine edit` on
  the project is E1-5b's open question.
- **The launcher:** `ofeliaengine edit` builds, copies, starts the editor in a job object
  (children stay in it) and returns when it is empty (`JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`,
  "not guaranteed": it also polls). A failed first build starts the last good slot.

## What we take

1. **Never run cargo's output.** Two slots, a third while one is held, with the game's own
   file name and its PDB: builds and the player's `cargo run` link freely, driver caches and
   firewall rules stay per known path, the last good build is always there.
2. **One build runner** in the editor: `cargo build --message-format=json` in the game's
   folder at below-normal priority (rustc and the linker inherit it), one build at a time,
   changes meanwhile queued (Dioxus); errors by primary span into the task panel as
   file:line:column links; `fresh` and the byte compare skip pointless restarts.
3. **Watching** with `engine_platform::fs_watch` and the shader hot reload's 10 ms settle:
   `src/`, `Cargo.toml`, `Cargo.lock`, `build.rs`, `.cargo/`, `rust-toolchain.toml`; cargo's
   dep-info (`<profile>/<game>.d`) lists every source, the engine's included.
4. **Spawn early, snapshot late**, with E1-5b's session files; `ofeliaengine edit` as the
   launcher, owning the console.
5. **A dev-like editor profile** if the numbers agree: dependencies without debug info, the
   game with line tables (Cargo book, ADR 0077), written by `ofeliaengine new`.
6. **Hot patching later on the same path:** a patch first, the restart for the rest: changed
   types, new components (a patch's `linkme` entries sit in another image), engine edits.
7. **The dev guide:** rust-analyzer in its own target directory (its FAQ), or its `cargo
   check` holds cargo's lock; a Dev Drive as an option.

## What we do not take

- **Kill first, then build or start** (cargo-watch, bacon, dx's full rebuild): the window
  goes for the whole build, and a failed build or a crash at start leaves nothing.
- **A new name per run** (Dioxus): AMD finds nothing in our cache file, one firewall prompt
  per run, files piling up.
- **Rename-aside as the default:** debuggers still block the PDB, a failed build leaves no
  exe. Measured against the copy, in case Defender's scan of copies costs.
- **`--artifact-dir`, a second target folder, nightly knobs:** nightly or a second engine
  build; the toolchain is pinned stable (ADR 0001); Cranelift aborts on panic on Windows.
- **The game or the engine as a DLL** (Fyrox, hot-lib-reloader, Bevy's dynamic linking):
  per-image `linkme` lists, no stable ABI. Only if the link dominates, with an ADR.
- **Fixed debounces** of 300–500 ms (cargo-watch, hot-lib-reloader), an editor that waits
  through a reload (Unity), Defender exclusions as advice (Microsoft: performance mode is
  "significantly better protection than ... folder exclusions").

## Measurement plan for E1-5c

On ref-1 (Ryzen 7 7700, RTX 4060 Ti, Radeon iGPU), never during a gate, nobody at the
desktop for the video; a game made by `ofeliaengine new` outside the repository, target on
the NVMe (as for the 2.5 s), editor feature on. Results into `docs/perf/history.md`.

1. **Edit to finished build**, for release, dev as `new` writes it, and dev with
   `[profile.dev.package."*"] debug = false` and the game at `"line-tables-only"`:
   - the first build once (the cost of switching), a build with nothing changed, 10 body
     edits (a constant in `tick`, two values in turn), 5 new struct fields, 5 comment edits;
   - a PowerShell script writes the edit and starts `cargo build --profile <p> --features
     editor --message-format=json`: one `Stopwatch` to `build-finished`, median and maximum;
   - once per profile: `--timings` for the game unit; `cargo rustc ... -- -C link-arg=/time`
     for lld's phases (only the game crate gets the flag); exe and PDB sizes; the `dyn Game`
     boundary as one more row once it exists;
   - the editor's frame time (p50, p99) during a build, cargo at normal and below-normal.
2. **Finished build to the new editor working:**
   - QPC stamps (one clock for both processes) at t0, copy, `main`, window, device,
     swapchain, pipelines, scenes, session, first frame, input taken; 10 restarts, median
     and maximum, against the 1 s of ADR 0107 §8;
   - a fresh copy against a second start of the same copy (Defender's share), and Defender's
     own account (`New-MpPerformanceRecording`, `Get-MpPerformanceReport -TopFiles`);
   - a used slot against a new one; RTX and `ENGINE_ADAPTER=radeon`; the quick start and
     10,000 entities;
   - no flash: the desktop recorded with ffmpeg's `ddagrab` at 120 fps
     (`D:/Environment/ffmpeg`), stepped through around each swap; place and focus kept;
   - failures: a build whose `settings()` panics, one that exits at once: the old editor
     back within its timeout, the message shown.
3. **Edit to working again** = 1 + 2, per profile.

## Open questions for the team

1. **The editor's profile** (release, `new`'s dev, or dev without the dependencies' debug
   info), chosen from the numbers. Dev-like means game code at opt-level 1 during Play, the
   engine still at 3: build speed against game speed (CLAUDE.md §3). Hot patching later
   needs `debug_assertions` anyway.
2. **Restart by itself, or offer it** (status bar, a shortcut) by default? A build that
   finishes during Play: wait for Stop, or stop Play?
3. **Binaries in `.ofelia/run/`** (2–3 slots, each an exe and its PDB): fine there?
4. **`&mut dyn Game` inside `Player::frame_overlaid` and `engine_editor::run`:** a small
   public API change, if the numbers show the game crate's codegen matters.
5. **A Dev Drive** on ref-1's second NVMe for game targets, and in the dev guide?
6. **Engine edits:** rebuild on them too (the dep-info list), or only on the game's files?
