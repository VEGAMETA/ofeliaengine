# Bootstrap shaders

`triangle.slang` is compiled to `triangle.vert.spv` / `triangle.frag.spv` (SPIR-V 1.6) with
Slang 2026.12:

```
slangc triangle.slang -target spirv -profile spirv_1_6 -entry vertexMain   -stage vertex   -O2 -fvk-use-entrypoint-name -o triangle.vert.spv
slangc triangle.slang -target spirv -profile spirv_1_6 -entry fragmentMain -stage fragment -O2 -fvk-use-entrypoint-name -o triangle.frag.spv
```

The binaries are committed only until the shader pipeline (`engine_shader`, CLAUDE.md §9)
compiles Slang automatically with a cache; then they are generated, not stored.
