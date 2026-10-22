# 0118. The shipping profile unwinds

- Status: Accepted (2026-10-22, team decision: option 1). It changes the stack's
  shipping profile (CLAUDE.md §15, which said `panic = "abort"`).
- Date: 2026-10-22
- Builds on: [ADR 0032](0032-scripting.md) (Luau through mlua),
  [ADR 0114](0114-luau-api.md) (the script host), [ADR 0061](0061-logging-crash-reports-gpu-faults.md)
  (crash reports)

## Context

- **What players get** is built with the `shipping` profile: `lto = "fat"`,
  `codegen-units = 1`, `panic = "abort"`. Every other profile (dev, test, release,
  profiling) unwinds.
- **Luau's errors are C++ exceptions.** mlua-sys builds Luau with exceptions (luau0-src's
  default; longjmp is not chosen). An error a Rust callback returns is raised with
  `lua_error` from mlua's `C-unwind` callback frame, and the exception unwinds through it
  back to Luau.
- **Under `panic = "abort"`, that unwinding aborts the process.** Rust turns a foreign
  unwind into a `C-unwind` frame into an abort when panics abort. Measured on ref-1
  (2026-10-22) with a probe built in the shipping profile:
  - `error("too fast")` in a script, and a library error (`string.rep(nil, 2)`), come
    back as script errors: they unwind through C++ frames only;
  - an error returned by a Rust callback aborts the process with "panic in a function
    that cannot unwind";
  - the engine's script host, built so, aborted at its first binding error (a component
    the entity lacks).
- **What that means:** in a shipped game, a script that reads a component its entity
  lacks, writes a value of the wrong type, or hits any other error inside an engine
  binding takes the whole game down, where every other build stops that one instance and
  says why in the log (ADR 0114 §9).

## Options

1. **`panic = "unwind"` in `shipping`.** Script errors behave as in every other build.
   A Rust panic unwinds too, as it does in release builds today; the crash reporter
   (ADR 0061) keeps its panic hook.
2. **Build Luau with longjmp** (luau0-src's `use_longjmp`, through a patched mlua-sys):
   errors jump over the Rust frames instead of unwinding through them. That is sound
   only where those frames hold nothing with a destructor, which mlua does not promise
   for Luau (it chose `C-unwind` for it), and it means keeping a fork of mlua-sys.
3. **Keep `abort`** and accept that a script error ends a shipped game.

## Measurements

The same probe, both ways (ref-1, `shipping` profile, four alternating runs each):

| | `panic = "abort"` | `panic = "unwind"` |
|---|---|---|
| a binding's error | the process aborts | the instance stops, the game goes on |
| 1,000 doors (the script host before E7-1c's fast path), one tick | 2.045–2.068 ms | 2.115–2.168 ms (+3.5 %) |
| 100,000 transforms propagated | 1.036–1.053 ms | 1.036–1.055 ms (the same) |
| the probe's executable | 3.86 MB | 4.05 MB (+5 %) |

The difference is in code that goes through mlua's callbacks (their landing pads); the
engine's own code is the same. E7-1c's fast path takes most script accesses out of mlua,
which shrinks that share further.

## Decision

Option 1 (team decision, 2026-10-22): `shipping` unwinds. What players get then behaves
as every build the team tests. The cost is about 5 % of the executable and nothing
measurable outside mlua's callbacks.

- `Cargo.toml`'s `[profile.shipping]` sets `panic = "unwind"`; CLAUDE.md §15 says so.
- `tools/check.ps1 -Profiles` runs `engine_app`'s `script_errors` example built with
  `shipping`: an error an engine function returns, one the fast path raises and one in
  Luau itself each stop their instance, and the process ends normally. A profile whose
  panics abort fails the step.

## Consequences

- **Easier:** a script error in a shipped game stops its instance and is logged, as in
  the editor and in release builds.
- **Harder:** executables are about 5 % larger; mlua-heavy script code is about 3.5 %
  slower than it would be under `abort`.
- **To revisit:** if mlua comes to build Luau with longjmp and promises frames without
  destructors at `lua_error` (option 2 without a fork), `abort` can come back with a test
  that runs a binding's error in the shipping profile.
