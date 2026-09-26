# Shaders: writing, building, hot reload

The engine compiles shaders written in [Slang](https://shader-slang.org) to SPIR-V. You
never run a shader compiler by hand. The editor and the `engine` command line find your
shaders, compile them on all cores, cache the results, and recompile the right ones when
you save a file.

## 1. What you need

- The **Vulkan SDK** (it ships the Slang compiler library). The engine finds it through the
  `VULKAN_SDK` environment variable that the SDK installer sets.
- To use a different Slang build, set `ENGINE_SLANG_LIBRARY` to the full path of
  `slang-compiler.dll`.

Players never need the SDK: games ship precompiled SPIR-V.

## 2. Shaders and modules

- A **shader** is a `.slang` file with at least one entry point marked
  `[shader("<stage>")]`. Stages: `vertex`, `fragment`, `compute`, `task`, `mesh`. Every
  entry point in the file is compiled; you do not list them anywhere.
- A **module** is a `.slang` file without entry points. It is compiled only through the
  shaders that `import` it.

```slang
// shaders/lighting.slang: a module
module lighting;
public float3 lambert(float3 n, float3 l, float3 albedo) { return albedo * max(dot(n, l), 0.0); }
```

```slang
// shaders/lit.slang: a shader with two entry points
import lighting;

struct VertexOut { float4 position : SV_Position; float3 normal : NORMAL; };

[shader("vertex")]
VertexOut vertexMain(float3 position : POSITION, float3 normal : NORMAL) { /* ... */ }

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    return float4(lambert(normalize(input.normal), float3(0, 1, 0), 1.0), 1.0);
}
```

Entry points keep their names in SPIR-V (`vertexMain`, `fragmentMain`), so the renderer
and tools such as RenderDoc show the names you wrote.

## 3. Compile All Shaders

From the command line (the editor button does the same):

```sh
cargo run --release -p engine_cli -- shaders build shaders
```

```text
     63.1 ms  C:\game\shaders\lit.slang  (2 entry points, Miss)
      0.1 ms  C:\game\shaders\sky.slang  (2 entry points, Hit)
2 shader files, 4 entry points in 0.07 s (1 compiled, 1 from cache, 0 failed; up to 2 in parallel)
```

- Errors are printed with file, line and column, and the command exits with status 1.
- Options:
  - `-O`: optimize;
  - `-g`: debug info, so RenderDoc and Nsight show your Slang source;
  - `-D NAME=VALUE`: define (repeatable);
  - `-I DIR`: extra import directory (repeatable);
  - `-j N`: compile at most N files at once;
  - `--cache DIR`: cache location (default `target/shader-cache`).
- **The cache** is keyed by the content of the shader and of everything it imports, the
  options and the compiler version. Unchanged shaders are never recompiled, even after a
  clean checkout, as long as the cache directory is kept. Point the whole team at one
  shared directory to share compiled shaders.
- **Memory:** each file compiled in parallel uses its own compiler instance (~150 MB). The
  default is one per logical CPU, at most 8. Lower `-j` on machines with little RAM.

## 4. Hot reload

While the editor (or `engine shaders watch shaders`) is running, save any `.slang` file:

- The engine recompiles exactly the shaders that use it: the file itself if it is a
  shader, or every shader that imports it if it is a module.
- Each recompiled shader is handed to the renderer as soon as it is ready. After editing a
  module used by many shaders, the first ones update long before the last.
- **If your edit does not compile**, the error is shown and the previous version keeps
  running. Fix the file and save again.
- Saving without a real change (or undoing back to the same code) swaps nothing.
- New shader files, deleted ones and new directories are picked up automatically.

Typical time from save to new SPIR-V is under 100 ms for a single shader.

## 5. Troubleshooting

| Symptom | Fix |
|---|---|
| "Slang compiler library not found" | Install the Vulkan SDK, or set `ENGINE_SLANG_LIBRARY`. The message lists every path tried. |
| A shader is not built | It needs a `[shader("...")]` entry point, and its directory must not start with `.`. |
| Edits in an import directory are ignored | Pass that directory with `-I`. Import directories are watched too. |
