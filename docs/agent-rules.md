# Rules for agents working on ofeliaengine (from the coordinator)

Read this whole file before starting, and the repository's CLAUDE.md. (Canonical copy:
`docs/agent-rules.md` in the repository.)

## What matters most (the owner's goal)

- **A technically ideal engine core:** well-designed internals and speed proven by
  measurement on both reference GPUs (RTX 4060 Ti and a weak Radeon 2-CU iGPU). Developer
  convenience, the editor, Luau and tooling come after the core.
- **Decide by evidence:** research first (papers, talks, shipped engines, open code with
  licenses and patents checked), then options weighed in the ADR, then the choice measured.
  "It looks fine" is not an argument; numbers on both GPUs are.
- **Never make existing things worse:** golden images, the per-pass perf gate and the
  register rule below exist for that. A feature a view does not use must cost it nothing.
- **Responsiveness is a first-class goal** (CLAUDE.md §7): no added latency without a
  measured reason.
- **Team decisions to respect** (look them up when your task touches them):
  - no camera motion blur in gameplay, only on cinematic views (ADR 0067);
  - singleplayer has no loopback client/server; the player's camera is updated every
    rendered frame (ADR 0019 addendum);
  - networking: authoritative ticks + snapshot replication, RPC/events only for one-shot
    events, NAT traversal with UPnP → NAT-PMP/PCP → hole punching → relay (planned);
  - patented algorithms are not used (e.g. Burley's diffusion profile, ADR 0059);
  - NVIDIA's DLLs are never shipped (ADR 0063); only permissive licenses (no GPL).
- **When unsure between two designs, pick the one that is measurably faster on both GPUs
  and simpler to reason about, and say why in the ADR.** Questions only the team can answer
  (licensing, patents, product decisions) go under "Questions for the team", not guessed.

## Setup

- Main repo: `H:\Desktop\ofeliaengine` (GitHub VEGAMETA/ofeliaengine). **Never touch the main
  checkout's working tree**; work in your own worktree:
  `git -C H:/Desktop/ofeliaengine fetch && git -C H:/Desktop/ofeliaengine worktree add H:/Desktop/ofeliaengine-wt/<task> -b feat/<task> origin/main`
- **Your own target dir**, never shared (cargo keys workspace crates by relative path; parallel
  builds from other worktrees corrupt each other): `export CARGO_TARGET_DIR=H:/Desktop/ofeliaengine-wt/<task>-target`.
- Test scenes: a worktree has no `test-assets/`; set
  `export ENGINE_TEST_ASSETS='H:\Desktop\ofeliaengine\test-assets'` (golden tests, perf gate) and
  pass absolute paths to examples (`H:/Desktop/ofeliaengine/test-assets/Sponza/Sponza.gltf`).
- Slang: `export ENGINE_SLANG_LIBRARY='C:\Users\VEGAMETA\AppData\Local\Temp\claude\H--Desktop-ofeliaengine\182125f2-d6a0-4fea-926e-6890c4402323\scratchpad\slang-2026.18.3\bin\slang-compiler.dll'`.
  Shaders are committed SPIR-V: after `cargo build -p engine_cli`, rebuild with
  `$CARGO_TARGET_DIR/debug/ofeliaengine.exe shaders build -O --out crates/engine_render/shaders crates/engine_render/shaders`.
- `cargo --offline` where possible (fetch a new crate once online if needed; justify every new
  dependency: why, size, compile time, license, maintenance — CLAUDE.md §15; `tools/check.ps1 -Deny`).
- GPUs: RTX 4060 Ti (default) and a Radeon RDNA2 2-CU iGPU (`ENGINE_ADAPTER=radeon`). Measure on both.
  Benchmarks: release, `VK_LOADER_LAYERS_DISABLE=~implicit~`.

## Before the PR

- `powershell -NoProfile -ExecutionPolicy Bypass -File tools/check.ps1 -Gpu -Perf` (plus `-Deny`
  for dependency changes, `-Loom` for lock-free code) must be green, with your target dir and
  ENGINE_TEST_ASSETS set.
- **Golden images** (`tests/golden`, ADR 0062): existing shots must stay identical unless your
  change is meant to alter the picture; then look at the new renders in `target/golden/<vendor>/`,
  rewrite them with `ENGINE_GOLDEN_UPDATE=1` on both GPUs and explain why in the PR. A new
  rendering feature adds its own shot.
- **Perf gate** (`check.ps1 -Perf`): no pass may get slower. If a pass got faster or slower on
  purpose, update the baseline (`perf_gate --update`, both GPUs) and record numbers in
  `docs/perf/history.md`.
- **Shader registers (hard rule, learned the hard way):** a feature must add no code to the
  shaders of views/materials that don't use it. On the AMD driver even dead code under a
  specialization constant costs VGPRs (forward shading sits at exactly 64; 72 made it 9–13 %
  slower). Use separate entry points with literal arguments, lazily created pipelines, or keep the
  parameter outside the shader. Check with `ENGINE_PIPELINE_STATS=1 ENGINE_ADAPTER=radeon
  scene_bench --sky --pipeline-stats` (ps numUsedVgprs per variant: 48/64/64/80 on main).
- No per-frame heap allocations (tests exist; add one for new per-frame paths); `// SAFETY:` on
  every unsafe block; LF line endings (Python on Windows: `newline='\n'`; check `git ls-files --eol`);
  rustfmt, clippy `-D warnings`.
- Docs per CLAUDE.md: ADR (number given in your brief; keep `docs/adr/README.md` rows sorted),
  research note `docs/research/<topic>.md` with sources (papers, talks, code + licenses, checked
  date), dev guide section, `docs/perf/history.md` row, THIRD_PARTY_NOTICES for adapted code.
  Allowed licenses: MIT, Apache-2.0, BSD, Zlib, ISC, Unicode, MIT-0, public domain. No GPL/LGPL.
  Check patents for algorithms where relevant and report them (the team avoids patented ones).

## Git and PR

- Commits end with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01QCm42AkZvxQc99y65ET4PR
  ```
- PR body sections: Done / Numbers / Decisions / Not done / Next / Questions for the team /
  Verification, ending with:
  ```
  🤖 Generated with [Claude Code](https://claude.com/claude-code)

  https://claude.ai/code/session_01QCm42AkZvxQc99y65ET4PR
  ```
- **Push your branch after every commit** (`git push -u origin feat/<task>` the first time, then
  `git push`; never force): local-only work is lost if the machine dies.
- `gh pr create` (not draft). Never force-push, never push to main, **never merge** (the
  coordinator merges).
- Final report to the coordinator (English, concise): PR URL, what was built, numbers (both GPUs),
  decisions, what's deferred, questions.
