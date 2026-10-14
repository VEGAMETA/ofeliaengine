# Testing pictures and GPU time

Two checks guard rendering (ADR 0062): golden images catch a changed picture, the
performance gate catches a slower pass. Both run on each GPU of the reference machine.

## The checks before a merge

`tools\check.ps1` runs everything a change must pass (ADR 0037, ADR 0077); `-Gpu` adds the
tests of every crate that links the RHI on both GPUs, `-Perf` the performance gate, `-Deny`
the dependency checks. Tests run under [cargo-nextest](https://nexte.st), once per machine:

```
cargo install cargo-nextest --locked
```

- **One build per check.** Every test step filters the same workspace build; a test runs
  in its own process, all binaries in parallel. Run one test with
  `cargo nextest run -E 'test(name)'`, a crate's with `-E 'package(engine_render)'`, or use
  `cargo test` as always.
- **Between commits**, `tools\check.ps1 -Gpu -Affected` runs only the tests of the packages
  your branch changed and of those that depend on them (`affected:` in the first line says
  which). The full run gates the merge.
- **Doc tests and formatting** follow the branch's changes in every run: unchanged code
  passed them on main.
- **A crate's integration tests are one binary**: `tests/<crate>/main.rs` declares each
  test file as a module (`mod forward;` for `tests/<crate>/forward.rs`); tests that count
  allocations share `tests/zero_alloc/main.rs` and its counting allocator. A new test file
  is a new module there, not a new binary (each binary links the whole engine).
- **Builds go to `C:\cargo-target\ofeliaengine`** (`.cargo/config.toml`): the NVMe drive
  rebuilds four times faster than the HDD the repository is on. Set `CARGO_TARGET_DIR` to
  build elsewhere; an agent's worktree builds in `C:\cargo-target\<task>`.
- **Test builds carry line tables only** (`[profile.test]`): file and line in panics and
  backtraces. To see variables in a debugger, build the test with
  `CARGO_PROFILE_TEST_DEBUG=full`.
- **New crates depend on `engine_workspace_hack`** (directly, or through `engine_core`).
  It gives the shared dependencies one set of features in every build, so `cargo test -p`
  a crate reuses what the full check built instead of rebuilding it. When `-Deny` reports
  feature drift after a dependency change, add the dependency it names there.

## Golden images

`tests/golden/tests/golden.rs` renders fixed shots and compares each with the golden
image of the GPU's vendor (`tests/golden/images/nvidia/`, `amd/`) through FLIP, a
perceptual difference:

```
cargo test -p engine_golden --test golden -- --nocapture golden::
set ENGINE_ADAPTER=radeon & cargo test -p engine_golden --test golden -- golden::
```

- **A test fails** when the mean FLIP exceeds 0.01 or the 99.9th percentile pixel 0.25.
  For scale: +3 % exposure is a mean of about 0.05; the sun turned by 1° about 0.02.
- **Look at what changed:** every run writes the render and FLIP's error map (black is
  equal, yellow is very different) to `target/golden/<vendor>/<shot>.png` and
  `<shot>.flip.png`.
- **You changed the picture on purpose:** look at the new renders first, then write them as
  the golden images on both GPUs and commit them with the change:

  ```
  set ENGINE_GOLDEN_UPDATE=1
  cargo test -p engine_golden --test golden -- golden::
  set ENGINE_ADAPTER=radeon & cargo test -p engine_golden --test golden -- golden::
  ```

- **Add a shot** for a new feature: a `Shot` in `golden.rs` that turns it on (see
  `tests/golden/src/scene.rs`: procedural scenes need no files; glTF scenes come from
  `test-assets/`, fetched by `tools/fetch-test-assets.ps1`, or from the folder in
  `ENGINE_TEST_ASSETS`: set it in a git worktree, which has no `test-assets/` of its own).
- **Compare any two images:** `cargo run -p engine_golden --release --bin flip -- a.png b.png map.png`.
- Missing golden images or scenes are skipped with a note, except in `tools/check.ps1 -Gpu`,
  which requires them.

## GPU time per pass

`perf_gate` renders the procedural scene and Sponza with each feature and antialiasing mode
at 1920 × 1080 (three runs of 100 frames per scene) and compares every pass's GPU time with
the baseline of the GPU's vendor (`tests/golden/perf/<vendor>.tsv`). Each device class's
defaults (`RenderDefaults`) are measured on Sponza and on Intel's Sponza, a dense scene of
3.7 M triangles. Intel's Sponza is not among the default test assets: fetch it with
`tools\fetch-test-assets.ps1 -Only IntelSponza` (5.4 GB of archives); without it its scenes
are skipped.

```
set VK_LOADER_LAYERS_DISABLE=~implicit~
cargo run -p engine_golden --release --bin perf_gate
```

- A pass is **slower** when it takes more than 3 % over its baseline and more than 5 µs or
  0.5 % of the frame on NVIDIA; 10 % and 2 % of the frame on AMD, whose iGPU's speed
  moves by several percent on an idle machine (small passes are noisy everywhere). The
  table marks it; `--strict` makes it an error.
- **Each scene runs three times from a fresh device, a round at a time** (the first run of
  every scene, then the second, then the third), and a pass counts its fastest run: a
  moment of other work on the machine slows one run of a few scenes, not every run of one.
- **A scene with a slower pass is measured again** (three more runs) and judged by the new
  runs: a passing slowdown is gone by then, a regression is not. The output lists the
  first runs' numbers and the scenes measured again.
- **Everything slower by the same few percent** is reported as a uniform shift, and a frame
  slower together with most of its large passes as a slower GPU: other clocks, the
  iGPU's memory state, or other work (a build, another test). Neither is counted.
  Measure again with the machine idle.
- **Keep the desktop still:** animated windows make the compositor redraw at the
  display's rate on its GPU (up to 30 % of the RTX's time), and the runs read slower.
  `tools\check.ps1 -Perf` warns when a GPU is busy before it measures.
- **The Radeon iGPU shares its die, power and memory with the CPU:** while every core
  works it renders about 10 % slower, and it is back to normal seconds after the load
  stops. Keep the CPU idle while it measures; `tools\check.ps1 -Perf` waits for ~12 quiet
  seconds before each GPU.
- **Overlays change timing:** keep `VK_LOADER_LAYERS_DISABLE=~implicit~` (OBS, RivaTuner and
  Steam hook into every Vulkan process) and close other GPU work.
  - In an elevated process (a terminal run as administrator) the loader ignores that
    variable. Each layer's own switch still works, from its manifest's
    `disable_environment`: `DISABLE_RTSS_LAYER=1`, `DISABLE_VULKAN_OBS_CAPTURE=1`.
  - `VK_LOADER_DEBUG=layer` lists the layers a process gets.
  - `tools\check.ps1` sets every registered overlay's switch for its GPU steps.
- **A change made a pass faster or slower on purpose:** `--update` writes the new baseline
  (each pass's median run, so that one lucky run does not set the bar); commit it with the
  change and put the numbers in `docs/perf/history.md`.
- `tools\check.ps1 -Perf` runs it on both GPUs and lists slower passes.
