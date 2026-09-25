# Third-party notices

Every third-party dependency, vendored source file, or adapted piece of code is listed
here with its license (CLAUDE.md §15). Update this file in the same commit that adds
the dependency.

## Rust crates (runtime dependencies)

_None yet._

## Rust crates (dev-dependencies only, not shipped)

| Crate | License | Used for |
|---|---|---|
| rayon 1.12 (+ rayon-core, crossbeam-deque, crossbeam-epoch, crossbeam-utils, either) | MIT OR Apache-2.0 | baseline in `engine_core` job-system benchmarks (ADR 0002) |
| loom 0.7 (only with `--cfg loom`) | MIT | model checking of the lock-free queues in `engine_core::jobs` |

## Vendored sources (`/third_party`)

_None yet._

## Adapted code

_None yet._
