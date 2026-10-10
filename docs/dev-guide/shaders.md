# Shaders: writing, building, hot reload

The engine compiles shaders written in [Slang](https://shader-slang.org) to SPIR-V. You
never run a shader compiler by hand. The editor and the `ofeliaengine` command line find
your shaders, compile them on all cores, cache the results, and recompile the right ones
when you save a file.

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

While the editor (or `ofeliaengine shaders watch shaders`) is running, save any `.slang` file:

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

## 6. Writing a compute pass

A compute pass is three things: the shader, a Rust struct for its push constants, and a
pass in the frame graph. `engine_render::compute` (ADR 0052) does the rest: creating the
pipeline, checking that the struct matches the shader, binding, pushing, and counting
workgroups.

**The shader** reads and writes resources through the bindless arrays and gets their
indices (and buffer addresses) as push constants:

```slang
// shaders/blur.slang
[[vk::binding(0, 0)]] Texture2D g_textures[];
[[vk::binding(1, 0)]] [vk::image_format("rgba16f")] RWTexture2D<float4> g_images[];
[[vk::binding(2, 0)]] SamplerState g_samplers[];

struct Push
{
    uint source;   // sampled image
    uint target;   // storage image
    uint sampler;
    uint2 size;
    float radius;
};
[[vk::push_constant]] ConstantBuffer<Push> push;

[shader("compute")]
[numthreads(8, 8, 1)]
void blur(uint3 id : SV_DispatchThreadID)
{
    if (any(id.xy >= push.size))
        return; // the last workgroups reach past the image
    // ...
    g_images[push.target][id.xy] = result;
}
```

**Reading through buffer addresses.** A buffer passed as an address is a pointer
(`float4* data`), and Slang declares a plain access `data[i]` only 4-byte aligned: some
drivers then split a 16-byte load into four. When the data is 16-byte aligned (its buffer
or ring allocation, and every element a multiple of 16 bytes), say so:
`float4 v = loadAligned<16>(push.data + i);` (`storeAligned<16>` to write). A single
4-byte value (`uint`, `float`) needs nothing. On the reference RTX this made the engine's
atmosphere pass 22 % faster (ADR 0013's addendum).

**The push constants in Rust**, field for field, `#[repr(C)]`, with `bytemuck` doing the
byte conversion (its derive refuses structs with hidden padding, so pad by hand):

```rust
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    source: u32,
    target: u32,
    sampler: u32,
    size: [u32; 2],
    radius: f32,
}

const BLUR: &[u8] = include_bytes!("../shaders/blur.blur.spv"); // built by `shaders build`
let blur = ComputeShader::<Push>::new(&device, &bindless, BLUR, c"blur")?;
```

- `ComputeShader::new` reads the workgroup size (`numthreads`) and the push constant
  block from the SPIR-V. If the shader reads more bytes than `Push` has (a field added in
  Slang and forgotten in Rust), it fails with a message naming both sizes, instead of the
  shader reading garbage. More than 128 bytes of push constants does not compile.
- With specialization constants: `ComputeShader::specialized(.., &[value_of_id_0, ..])`.

**Each frame**, declare what the pass reads and writes, then dispatch:

```rust
fg.add_compute("blur", &bindless)
    .read(hdr, Access::Sampled(ShaderStages::COMPUTE))
    .write(blurred, Access::StorageWrite(ShaderStages::COMPUTE))
    .dispatch(&blur, Dispatch::Threads([width, height, 1]), move |ctx| {
        Some(Push {
            source: ctx.sampled_index(hdr)?,
            target: ctx.storage_index(blurred)?,
            sampler: Bindless::sampler(SamplerKind::LinearClamp),
            size: [width, height],
            radius: 4.0,
        })
    });
```

- `Dispatch::Threads([w, h, d])` dispatches enough workgroups for that many threads (the
  shader must skip the extra ones, as above). `Dispatch::Groups([x, y, z])` gives the
  counts directly, when a workgroup covers something else than its threads (a tile, a
  light).
- The closure runs when the frame is recorded; `ctx` gives bindless indices of the pass's
  textures and addresses of its buffers. Returning `None` (`?` on a missing index) skips
  the dispatch.
- The graph puts the barriers between passes from what you declared.
- **Several dispatches in one pass** (for example a mip chain inside one image):

```rust
fg.add_compute("blur mips", &bindless)
    .write(chain, Access::StorageReadWrite(ShaderStages::COMPUTE))
    .record(move |rec| {
        for level in 1..levels {
            let size = [w >> level, h >> level, 1];
            rec.dispatch(&reduce, Dispatch::Threads(size), &Push { level, ..base });
            rec.barrier(); // the next level reads what this one wrote
        }
    });
```

  `rec.context()` gives the same indices and addresses as `ctx` above.
- No `unsafe` anywhere in pass code. Rebuild the SPIR-V after editing the shader
  (`shaders build`, section 3); hot reload of engine passes comes later.
- Examples in the engine: `postfx.rs` (bloom and exposure), `environment.rs` (mip chains,
  several shaders in one pass), `hiz.rs` (workgroups per tile), `engine_upscale` (TAAU,
  specialization constants).
- **Several shaders at once** compile in parallel (section 7):

```rust
let [down, up] = ComputeSource::create_all(&device, &bindless, [
    ComputeSource::<Push>::new(DOWN_SPV, c"down", &[])?,
    ComputeSource::<Push>::new(UP_SPV, c"up", &[])?,
])?;
```

## 7. Pipelines at start-up: the cache

The driver turns each shader, with its render state, into a pipeline: machine code for
your GPU. The engine's renderers make 53 of them. Compiled one after another they take
1–1.5 s, and the drivers' own caches do not survive a patch, a reinstall or a driver
update. The engine keeps its own cache (ADR 0055):

```rust
use engine_rhi::PipelineCacheLoad;

let mut device = Device::new(&instance, &adapter)?;
// Optional: compile on your job pool instead of threads started for it.
device.set_job_pool(jobs.clone()); // an Arc<ThreadPool>

// Before creating the renderers:
let cache = save_dir.join(device.pipeline_cache_file_name());
if let PipelineCacheLoad::Rejected(why) = device.load_pipeline_cache(&cache) {
    println!("pipeline cache not used: {why}"); // after a driver update, for example
}
let forward = ForwardRenderer::new(&device, &bindless, HDR_FORMAT, DEPTH_FORMAT, shadows)?;
// ... the other renderers ...

// Once start-up is done, and again at exit or after loading screens:
device.save_pipeline_cache(&cache)?;
```

- **What it costs** (all 53 pipelines, RTX 4060 Ti / Radeon iGPU):

  | | ms |
  |---|---|
  | with the cache | 7 / 6 |
  | after a driver update or on a new GPU (compiled in parallel) | 300 / 320 |
  | before this cache existed, cold | 1,430 / 1,020 |

- **Where to keep it:** in the player's local data, for example
  `%LOCALAPPDATA%\<game>\`, not in the installation folder (read-only under
  Program Files) and not in roaming profiles (megabytes, and only valid for this GPU and
  driver).
  - `pipeline_cache_file_name()` includes the GPU's ids, so a laptop with two GPUs keeps
    one file per GPU.
- **Never a crash or a wrong picture:**
  - a damaged file, one from another GPU or another driver is refused, with the reason;
    the game compiles as without a cache and the next save replaces the file;
  - saving writes a temporary file and renames it, so a crash mid-save keeps the old
    file;
  - saving an unchanged cache does nothing.
- **Your own pipelines** go through the cache by themselves. Create many at once with
  `Pipeline::create_many(&device, layout, &descs)`, or `ComputeSource::create_all` for
  compute shaders:
  - those in the cache come first, one after another (a lookup is a fraction of a
    millisecond, and drivers serialize them);
  - the others compile in parallel.
- **Measure:**
  - `cargo run -p engine_upscale --release --example pipeline_bench -- --cache <file>`;
  - `ENGINE_PIPELINE_SALT=<a new number>` makes every pipeline miss every cache: a cold
    start on demand;
  - `engine_rhi::creation_stats()` counts pipelines, cache hits and misses: log it, or
    show warm-up progress.
- The editor keeps its caches in `%LOCALAPPDATA%\ofeliaengine\editor` and prints the
  hits in its cold-start report (`editor --frames 60`).
- **Not yet:**
  - recording pipelines as they are used, which the material graph will need;
  - `VK_KHR_pipeline_binary`;
  - fast linking for pipelines created while drawing.
