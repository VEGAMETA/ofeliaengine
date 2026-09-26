# Scripting language and runtime

Checked: 2026-09-26. Sources were read for design ideas only; no code was carried over.

| Option | Version | License | Notes |
|---|---|---|---|
| Luau | 0.736 | MIT | Lua 5.1 descendant: gradual types, `vector` (3×f32 value type), `buffer`, sandboxing, native codegen (x64/arm64), interrupts, used at scale by Roblox |
| mlua | 0.12.1 | MIT | Rust bindings; `luau-jit` feature for native codegen; sandbox, interrupt, memory limit, compiler options |
| wasmtime | (not added) | Apache-2.0 | WASM runtime with Cranelift; typed integers and floats; heavier dependency and boundary cost |
| C / C3 as DLLs | — | — | native speed; no sandbox; slow reload; C3 tooling immature |

## Luau facts that matter for the engine

- `number` is always a double. There is no integer type, so integers are exact to
  2^53; `bit32` provides 32-bit bit operations.
- `vector` is a built-in value type (three f32 by default), with arithmetic and a
  `vector` library that native codegen specializes.
- `buffer` is a fixed-size byte array with `readf32`, `readi32`, `writeu16` and similar
  functions.
- Type annotations (`: number`, `: vector`) guide the checker (luau-lsp) and give
  native codegen hints (`set_type_info_level`); they do not change storage.
- Sandboxing (`luaL_sandbox`) makes globals and builtin metatables read-only and marks
  the environment "safe", which lets the compiler resolve builtins statically and use
  fast calls. A custom environment must keep that flag (`lua_setsafeenv`), or scripts
  run 16× slower (measured, ADR 0032).

## What we take

Luau through mlua (CLAUDE.md §10), with the measurements and the boundary-type rules
in ADR 0032. Rust modules for hot loops, WASM as a later option.
