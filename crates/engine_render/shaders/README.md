# Renderer shaders

Slang sources and the SPIR-V the renderer embeds (`include_bytes!`), so the engine
builds and runs without the Slang compiler. `tests/shaders.rs` recompiles the sources and
fails if the committed SPIR-V is stale (when the Slang version matches, see
`BUILT_WITH` there).

Rebuild after editing a `.slang` file:

```sh
cargo run -p engine_cli -- shaders build -O --out crates/engine_render/shaders crates/engine_render/shaders
```

Conventions (ADR 0012, ADR 0013):
- matrices are column-major in memory (glam's layout), and `mul(M, v)` applies them;
- set 0 is the bindless set (`g_textures`, `g_images`, `g_samplers`);
- push constants hold up to 128 bytes;
- buffers are reached through device addresses (`T*` pointers in Slang).
