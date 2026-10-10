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
- **New crates depend on `engine_workspace_hack`** (directly, or through `engine_core`).
  It gives the shared dependencies one set of features in every build, so `cargo test -p`
  a crate reuses what the full check built instead of rebuilding it. When `-Deny` reports
  feature drift after a dependency change, add the dependency it names there.

## Golden images

`tests/golden/tests/golden.rs` renders fixed shots and compares each with the golden
image of the GPU's vendor (`tests/golden/images/nvidia/`, `amd/`) through FLIP, a
perceptual difference:

```
cargo test -p engine_golden --test golden -- --nocapture
set ENGINE_ADAPTER=radeon & cargo test -p engine_golden --test golden
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
  cargo test -p engine_golden --test golden
  set ENGINE_ADAPTER=radeon & cargo test -p engine_golden --test golden
  ```

- **Add a shot** for a new feature: a `Shot` in `golden.rs` that turns it on (see
  `tests/golden/src/scene.rs`: procedural scenes need no files; glTF scenes come from
  `test-assets/`, fetched by `tools/fetch-test-assets.ps1`, or from the folder in
  `ENGINE_TEST_ASSETS`: set it in a git worktree, which has no `test-assets/` of its own).
- **Compare any two images:** `cargo run -p engine_golden --release --bin flip -- a.png b.png map.png`.
- Missing golden images or scenes are skipped with a note, except in `tools/check.ps1 -Gpu`,
  which requires them.

## GPU time per pass

`perf_gate` renders the procedural scene and Sponza at 1920 × 1080 (three runs of 200
frames) and compares every pass's GPU time with the baseline of the GPU's vendor
(`tests/golden/perf/<vendor>.tsv`):

```
set VK_LOADER_LAYERS_DISABLE=~implicit~
cargo run -p engine_golden --release --bin perf_gate
```

- A pass is **slower** when it takes 3 % more than its baseline and more than 5 µs or 0.5 %
  of the frame (small passes are noisy). The table marks it; `--strict` makes it an error.
- **Everything slower by the same few percent** is reported as a uniform shift: the GPU ran
  at other clocks or shared with other work (a build, another test). Measure again with
  the machine idle.
- **Overlays change timing:** keep `VK_LOADER_LAYERS_DISABLE=~implicit~` (OBS, RivaTuner and
  Steam hook into every Vulkan process) and close other GPU work.
- **A change made a pass faster or slower on purpose:** `--update` writes the new baseline;
  commit it with the change and put the numbers in `docs/perf/history.md`.
- `tools\check.ps1 -Perf` runs it on both GPUs and lists slower passes.
