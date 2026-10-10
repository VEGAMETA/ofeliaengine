//! Empty: Cargo resolves a package's build dependencies only when it has a build script,
//! and the `[build-dependencies]` of this crate unify the features of dependencies built
//! for the host (proc macros' `syn`) the way its `[dependencies]` do for the target
//! (ADR 0077).

fn main() {}
