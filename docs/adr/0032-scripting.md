# 0032. Scripting: Luau for game logic, Rust modules for hot loops, WASM later

- Status: Accepted (tiers agreed with the team 2026-09-26). The public scripting API is
  **provisional** until the team signs it off (CLAUDE.md §3: irreversible).
- Date: 2026-09-26
- Research: [docs/research/scripting.md](../research/scripting.md)

## Context

CLAUDE.md §10 fixes Luau (via `mlua`, native codegen, sandboxed), with a second tier of
Rust game modules for heavy logic. The team asked how Luau's types relate to engine
data and speed:
- Luau has one `number` type (a 64-bit double, integers exact to 2^53), a built-in
  3×f32 `vector`, and `buffer` with typed reads and writes.
- Type annotations serve the checker and give native codegen hints; they are not
  storage types.

Alternatives raised:
- C or C3 as the scripting language;
- WASM.

## Options

1. **C/C3 native DLLs.** Native speed, but:
   - no sandbox: a script bug takes the editor down (§10 forbids this);
   - reload is a DLL rebuild (seconds, not ≤ 100 ms);
   - C3 has no mature tooling.
   Native speed is already covered by the Rust module tier, which also shares the
   engine's types and safety.
2. **WASM (wasmtime, Apache-2.0).** Typed i32/i64/f32/f64, sandboxed, near-native,
   any source language. The runtime is heavy, the boundary costs something, and it is
   less friendly for designers. Kept for the future: fast, safe mods.
3. **Luau + Rust modules** (chosen).

## Measurements

`cargo bench -p engine_script --bench boids`, container (4 threads, not reference),
bench profile. 1,000 agents, all pairs (1M distance tests), one step:

| variant | ms | × Rust |
|---|---:|---:|
| Rust (f32 arrays) | 1.31 | 1.0 |
| Luau native, vectors in tables | 5.35 | 4.1 |
| Luau native, per-module environment | 84.04 | 64.3 |
| Luau native, per-module environment marked safe (`ScriptVm`) | 5.33 | 4.1 |
| Luau interpreted, vectors in tables | 26.93 | 20.6 |
| Luau native, f32 in buffers | 5.71 | 4.4 |
| Luau interpreted, f32 in buffers | 76.41 | 58.4 |

- **Native codegen matters:** it is 5× faster than the interpreter on vector code.
- **A per-module environment must be marked safe.**
  - Otherwise Luau assumes builtins may be replaced and turns off fast calls and import
    resolution: 16× slower.
  - `ScriptVm` sets the flag (`lua_setsafeenv`), as Luau's own `luaL_sandboxthread`
    does, so module isolation costs nothing.
- **`vector` values in tables beat `buffer` reads** under native codegen. Buffers are
  for bulk transfer (ECS columns), not for math.
- **About 4× Rust on a numeric hot loop** is the price of Luau. Game logic with many
  entities per frame belongs in Rust modules. Event-driven logic belongs in Luau.

## Decision

- **`engine_script::ScriptVm`:**
  - `mlua` 0.12 with `luau` and `luau-jit`, sandboxed standard library;
  - compiler at optimization level 2 with type info;
  - per-module environment, read-through to the standard library, marked safe.
- **Modules:**
  - a chunk receives a persistent `state` table and returns a table of functions;
  - reload keeps `state` and calls `on_reload(old)`;
  - a broken edit keeps the old code and reports its line.
- **Limits:**
  - a per-call time budget (default 50 ms) through Luau's interrupt: runaway loops
    return `ScriptError::Timeout` and the VM stays usable;
  - a memory limit (default 256 MiB).
- **Errors:** `ScriptError` carries the module and line for the editor.
- **Types at the boundary** (bindings, next step):
  - `Vec3` ↔ `vector`;
  - `f32`, `i32` and `u32` fields ↔ `number`, checked and converted at the boundary,
    with ranges from reflection attributes and documented in the generated
    `.d.luau`;
  - 64-bit values and `Entity` as userdata (not `number`, to avoid precision loss);
  - bulk component data as `buffer`.
- **Tiers:**
  - Luau: gameplay logic, UI, quests, tools, mods;
  - Rust modules (hot patching, §10): hot loops;
  - WASM: a future option for fast sandboxed mods.

## Consequences

- One scripting language, sandboxed, with sub-100 ms reloads and line-accurate errors.
- Performance guidance for game developers is concrete: a loop over thousands of
  entities per frame goes to Rust; the dev guide says so with these numbers.
- Next steps:
  - reflection-generated bindings and `.d.luau`;
  - Tracy zones for script calls;
  - the DAP debugger;
  - file-watching hot reload in the editor.
