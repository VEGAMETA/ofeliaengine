# 0111. Games share one build of the engine

- Status: Accepted (2026-10-21): the shared build; the engine as a DLL measured, left for
  hot reload of game code
- Date: 2026-10-21
- Changes: what `ofeliaengine new` writes (the quick start, #249; the template's profile,
  #288)

## Context

The owner, after trying the editor (2026-10-21): building the whole engine again for every
game is slow. They asked for the engine's compiled libraries taken from where the engine
is installed, through a DLL; 2–3 GB for the engine is fine.

Until now `ofeliaengine new` made a game that built the engine from source in its own
`target`, with the engine's crates as path dependencies and their dependencies resolved
afresh into the game's own `Cargo.lock`. Measured on ref-1 (Ryzen 7 7700, NVMe, Rust
1.94.1) on a fresh game from the template:

| | today |
|---|---:|
| first build, playing | 101.8 s |
| first build with the editor, after it | 19.7 s |
| a one-line change, with the editor | 1.5 s |
| the same, playing | 1.3 s |
| the game's `target` | 1.9 GB |

- **A rebuild after a change** is 0.21 s of cargo and 1.11 s of the game's crate (cargo's
  `--timings`). The link inside that is 0.25 s (lld's `/time`).
- **A fresh game's lock took newer versions** of 16 of the engine's dependencies than the
  engine's own lock, versions the engine was never tested with: `cc` 1.9.0 for 1.5.1,
  `glam` 0.33.12 for 0.33.10, `uuid` 1.28.0 for 1.26.1, and others.

## Options

1. **Each game its own build** (until now).
2. **One build of the engine shared by every game.**
   - Cargo's `build.build-dir` (stable since 1.91) puts the intermediate files in one
     folder per user; each game's program stays in its own `target`.
   - The engine's `Cargo.lock` is copied into the game, so every game on one engine
     resolves the same versions.
   - Cargo's units are keyed by the package's path and version, its features, the profile
     and the compiler, so the engine's are the same for every game and found fresh.
3. **The engine as one DLL**, linked by the game's development builds in the shared build:
   a `dylib` crate over `engine_app` and `engine_editor`, as `bevy_dylib` does for Bevy.
4. **A compiler cache** (sccache). Each game's build still runs every unit, served from the
   cache. It is a tool to install, and the C++ that the build scripts compile is outside
   its rustc wrapper.
5. **A prebuilt SDK passed to rustc** (`--extern` and `-L` in rustflags, and no cargo
   dependency on the engine). Cargo would not know the engine, so rust-analyzer could not
   complete its API, and a game adding a crate the engine uses would get a second copy of
   that crate.

## Measurements (ref-1, 2026-10-21)

Options 2 and 3 were tried on games from the template with `build-dir` and the engine's
lock added by hand; the DLL as a `dylib` crate outside the repository, linked under the
game's `editor` feature.

| | 1. own build | 2. shared build | 3. shared, DLL |
|---|---:|---:|---:|
| the first game's first build, playing | 101.8 s | 95.7 s | |
| with the editor, after it | 19.7 s | 19.4 s | |
| **a second game's first build, playing** | 101.8 s | **3.4 s** | |
| **with the editor, after it** | 19.7 s | **4.0 s** | 5.1 s (the DLL linked) |
| a change, with the editor | 1.5 s | 1.52–1.55 s | 1.49–1.52 s |
| a change, playing | 1.3 s | 1.3 s | |
| the game's `target` | 1.9 GB | 31 MB | 31 MB, and a copy of the DLL |
| the shared folder | | 2.0 GB | and the DLL, 43 MB |
| the program | 22.5 MB | 22.5 MB | 0.57 MB, and the DLL |
| reflected types the game sees | 22 of 22 | 22 of 22 | **21 of 22: not the game's own** |
| the program started outside cargo | runs | runs | **does not start**: `std-*.dll` not found |

**The template itself, end to end** (`ofeliaengine new` after this change, two fresh
games): the first game took 90.5 s, and 19.2 s with the editor, and filled the shared
folder. The second took 3.3 s and 4.0 s, and its `target` holds 32 MB.

The DLL fails in three ways:
- **No real saving:** it saves 0.03 s of a 1.5 s rebuild. The link is a quarter of a second
  either way; the rest is the game's crate.
- **The game's own components disappear:** the registry of reflected components
  (`engine_ecs::reflected`) is a linkme distributed slice, and those are collected per
  module. The DLL's registry does not see the components in the program, so the inspector,
  scenes, saves and networking would lose them.
- **The program does not start outside cargo:** a DLL dependency makes rustc link std as a
  DLL too, because cargo passes `-C prefer-dynamic` to a `dylib` crate. That DLL is in the
  toolchain's folder, which only `cargo run` puts on `PATH`. The editor runs copies of the
  program in its slots.

A DLL that a program has loaded is also locked: the engine could not be rebuilt while any
game ran from it.

## Decision

Option 2 (team decision, 2026-10-21): the owner's goal, the engine compiled once and taken
from one place; the mechanism by the coordinator's measurements.

- **`ofeliaengine new` writes** `build-dir = "{cargo-cache-home}/ofeliaengine/build"` in the
  game's `.cargo/config.toml`. That is a folder per user, on the drive of cargo's home
  (`%USERPROFILE%\.cargo` unless `CARGO_HOME` says otherwise), and it holds on every machine
  of a team that commits the config.
- **It copies the engine's `Cargo.lock`** into the game.
- **The build is not in the engine's folder:** on ref-1 that is on the HDD, where builds take
  about four times as long (ADR 0077), and an installed engine's folder (later) is not
  writable. An installer can fill the shared folder when it installs.

**The DLL (option 3) waits for hot reload of game code** (CLAUDE.md §10: hot patching Rust
game code in at most 3 s; subsecond and hot-lib-reloader compared in its ADR). There the game
becomes a DLL that the editor reloads in place, and the engine a DLL both share. It needs
what this measured:
- a registry per module, each module's slice registered with the engine's when it loads;
- std's and the engine's DLLs beside the editor's slots;
- the engine's DLL never rebuilt under a running game.

## Consequences

- **Speed and disk:** a new game builds in seconds once any game on the machine has built
  the engine. A game's `target` holds only its own files (31 MB, not 1.9 GB).
- **Versions:** every game on one engine builds the dependency versions the engine is
  tested with.
- **`cargo clean`:** a bare `cargo clean` in a game removes the shared build, because cargo
  cleans its `build-dir`; `cargo clean -p <game>` cleans only the game's files. The
  template's config and README say so.
- **Builds of two games at once take turns:** cargo locks the build folder for a build.
- **Games made before this** keep their own build until their `.cargo/config.toml` gets
  the `[build]` lines. To get the engine's versions too, they also need the engine's
  `Cargo.lock`.
- **After an engine update** that changes the engine's `Cargo.lock`, a game keeps its own
  lock's versions where they still fit. Copying the engine's lock again brings it back to
  the shared versions.
- **Shipping builds are unchanged** (`--profile shipping`): one static program.
