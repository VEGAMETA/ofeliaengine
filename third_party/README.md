# Third-party sources

Source only, unmodified, each with its license (CLAUDE.md §2, §15). Every entry is also
listed in `THIRD_PARTY_NOTICES.md`.

| Directory | Project | Version | License | Used by |
|---|---|---|---|---|
| `JoltPhysics/` (only `Jolt/` and `LICENSE`) | [Jolt Physics](https://github.com/jrouwe/JoltPhysics) | v5.6.0, commit `e77f175595e64cb44218cc9d9d56fc365ad0e36a` | MIT | `engine_physics_jolt` |
| `joltc/` (only `include/`, `src/`, `LICENSE`) | [JoltC](https://github.com/amerkoleci/joltc) | commit `886e088675bae3a086f8318c7803f8ee962c2f2c` (builds Jolt v5.6.0) | MIT | `engine_physics_jolt` |

To update, replace the directory with the new version's files, record the version and
commit here, and run the `engine_physics_jolt` tests: the FFI layout test catches struct
changes.
