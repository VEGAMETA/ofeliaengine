# 3D rendering: meshes, materials and many objects

The forward renderer draws on the GPU's initiative (ADR 0034). You hand it a list of
objects each frame, each a mesh, a material and a transform. The GPU then:

- throws away what the camera cannot see;
- draws everything that shares a mesh and a kind of material with one instanced draw
  call.

The CPU cost of a frame barely depends on how many objects the list holds.

## How a frame is put together: the frame renderer

`engine_frame::FrameRenderer` composes a whole frame for each view (ADR 0072). Use it
instead of wiring the passes below by hand: it puts them in the right order, at the
right resolution, with the right antialiasing.

```text
 scene passes (render size)              antialiasing               post (output size)
 cull → shadows → prepass/AO → forward   TAAU / FSR / DLSS          bloom → tone map
 → SSS → sky/fog → transparent           (Off: nothing)             → your target
 → skinned motion; exposure measured
 jitter + motion vectors ──────────────▶ history per view
```

```rust
use engine_frame::{
    Antialiasing, FrameContext, FrameRenderer, PrepareContext, Quality, RenderDefaults,
    SceneDraws, SceneInput, View, ViewCamera, ViewTarget,
};

// Once: the passes' pipelines, for the swapchain's format.
let frame = FrameRenderer::new(&device, &bindless, swapchain.format(), vk::Format::D32_SFLOAT,
    ShadowSettings::default(), frames_in_flight)?;
// Measured defaults for this GPU (discrete or small integrated): AO, antialiasing, size.
let defaults = RenderDefaults::for_device(&device);
frame.forward_mut().set_ambient_occlusion(&device, &bindless, defaults.ambient_occlusion)?;
let mut settings = defaults.view_settings();          // or ViewSettings { .. }
// Once per view (main camera, each split-screen player, each render target):
let mut view = View::new(&device, 100_000);

// Every frame, before materials.begin_frame (the LOD bias lives in the samplers):
frame.prepare(&mut view, &settings, (width, height), PrepareContext {
    device: &device, allocator: &mut allocator, bindless: &bindless,
    materials: &mut materials, frame_number: frame_number })?;
// In the frame's graph, after the pools' begin_frame (and GpuScene::add_passes,
// Skinning::begin_frame):
frame.add_view(&mut fg, &mut FrameContext { device: &device, allocator: &mut allocator,
        bindless: &bindless, ring: &mut ring, frame_number, dt },
    &mut view,
    &SceneInput { geometry: &geo, materials: &mats, draws: SceneDraws::Scene(&scene_frame),
        skinning: Some(&skinned), light: sun, lights: &lights, environment: Some(&sky) },
    &ViewCamera { view_from_world, position, fov_y, near, far, cut: false },
    ViewTarget { texture: backbuffer, rect: None, load: LoadOp::DontCare })?;

// At shutdown, with the device idle:
unsafe { view.destroy(&mut allocator, &bindless) };
```

- **Antialiasing** (`ViewSettings::antialiasing`):
  - `Antialiasing::OFF`: no jitter, no motion vectors, no history: the cheapest, and
    aliased;
  - `Antialiasing::TAAU`: the engine's own; at `Quality::Native` this is plain TAA;
  - `Antialiasing::FSR`: AMD FSR 3.1, on any GPU (built in);
  - `Antialiasing::DLSS`: NVIDIA DLSS (DLAA at `Native`), once you hand the frame renderer
    Streamline (`frame.set_streamline(Some(streamline))`, see below). Without it, the view
    falls back to TAAU; `prepare` returns what is used.
  - and every other mode — MSAA, FXAA, MLAA, SMAA (1x, S2x, T2x, 4x), nearest, bilinear,
    integer scaling, FSR 1 — and their combinations: see
    [Choosing antialiasing](#choosing-antialiasing-every-mode-what-it-costs-what-it-gives)
    (ADR 0075).
- **Render size** (`ViewSettings::quality`): `Native` (100 %), `Quality` (67 %), `Balanced`
  (59 %), `Performance` (50 %), `UltraPerformance` (33 %) per axis. With `Off`, a smaller
  size is stretched bilinearly. `ViewSettings::render_size` fixes it in pixels instead
  (pixel art).
- **Dynamic resolution:** set `ViewSettings::render_scale = Some(0.8)` frame by frame
  (from your GPU-time controller). The texture LOD bias stays the one of `quality`; keep
  the scale at or below it and change it in coarse steps.
- **Camera cuts:** set `ViewCamera::cut` on the first frame of a new shot (teleport,
  another camera, a new level). The history, last frame's motion, the occlusion state and
  the adapted exposure start over: nothing of the old picture ghosts into the new one.
  Changing the upscaler or the output size does the same by itself.
- **Exposure** (`ViewSettings::exposure`): `Exposure::Fixed(1.0)` or
  `Exposure::Auto(ExposureSettings { .. })`. It is measured on the scene before
  antialiasing, and FSR and DLSS get the same exposure the tone mapper applies (+0.3 to
  +0.8 dB of image quality against their own measurement).
- **Passes of your own on the scene** (screen-space reflections, debug views):
  `frame.add_view_with(.., |fg, targets| { .. })` calls you between the scene passes and
  antialiasing with `targets.color`, `depth`, `motion` (motion vectors, jitter, reset,
  reactive mask) and, with `ViewSettings::previous_color`, last frame's antialiased HDR
  image (`targets.previous_color`, `None` after a cut). What you write into the color is
  antialiased and post-processed like the rest.
- **Several views:** one `View` each, `add_view` for each into the same graph, with
  `ViewTarget::rect` for split screen (the first clears, the others `LoadOp::Load`).
  Views drawing with one `MaterialTable` share its texture LOD bias (the last prepared
  wins): give them the same antialiasing.
- **With the game runtime:** put an `engine_runtime::Camera` on an entity and let a
  `CameraView` resolve it every frame (it gives the `ViewCamera` and the settings); move
  the player's camera in `Runtime::frame`'s presentation step. See
  [ecs.md](ecs.md#cameras).

What the choices cost and give on Sponza at 1080p (ref-1, `cargo run -p engine_golden
--release --example frame_bench`; FLIP against the frame rendered at 4K and averaged 2×2,
lower is better):

| mode | RTX 4060 Ti ms | Radeon iGPU ms | FLIP |
|---|---:|---:|---:|
| off, 1080p | 0.78 | 17.1 | 0.024 |
| TAA (`TAAU`, `Native`) | 0.9–1.3 | 23.2 | 0.032 |
| TAAU 67 % (`Quality`) | 0.74 | 16.3 | 0.041 |
| TAAU 50 % (`Performance`) | 0.67 | 13.8 | 0.049 |
| FSR 3.1 Native AA / Quality | 1.71 / 1.17 | 42.1 / 25.3 | 0.033 / 0.043 |
| DLAA / DLSS Quality | 3.11 / 1.62 | – | 0.018 / 0.023 |

- On a small integrated GPU, TAAU from 67 % is *cheaper* than no antialiasing at full
  size; FSR costs more than the whole scene there. The defaults follow this
  (`RenderDefaults`): TAA and AO High on discrete GPUs, TAAU 67 % and AO Low on integrated
  ones.
- A still image rewards sharpness: TAAU and FSR soften textures a little (their FLIP is
  higher than no antialiasing's), while removing the shimmer of edges and foliage in
  motion that a still image does not show. DLSS does both.
- Ambient occlusion runs at the render size: at 67 % it costs 2.3× less (High: 12.1 →
  5.4 ms on the Radeon) for the same quality. See section 13.

What the composer itself costs on the CPU: `prepare` + `add_view`, declaring every pass
of the frame, take 4–10 µs per view and frame on ref-1 (20,000 instances in a GPU scene,
`game_loop`), and allocate nothing.

## 1. Meshes

Every mesh lives in one `GeometryPool` (ADR 0039): shared buffers of vertices and indices,
so the whole draw list is drawn with a few indexed indirect draws.

```rust
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshData};

let mut geometry = GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), IN_FLIGHT)?;
let rock_mesh = geometry.add_mesh(&MeshData {
    positions: &positions,
    normals: &normals,
    tangents: &tangents, // or &[] without a normal map
    uvs: &uvs,           // or &[]
    indices: &indices,   // triangles, counter-clockwise front faces
})?;

// Every frame, once, before the passes that draw:
let geo = geometry.begin_frame(&mut fg, &device, &mut allocator, frame.number)?;
```

- **Add meshes whenever you like.** Their data reaches the GPU in the next `begin_frame`;
  the pool grows by itself (the buffers are replaced and the old contents copied on the
  GPU).
- **Remove with `remove_mesh(id, frame.number)`.** Draws of the id then draw nothing, and
  its memory is reused once the frames that could still draw it are done.
- **Indexed meshes, as imported:** a vertex shared by six triangles is stored and shaded
  once.
  - On Sponza (262k triangles) that is 4× fewer vertices than a triangle list, 8 MB
    instead of 24, and a forward pass half as long.
- **Vertex size:** 28 bytes (position 12; normal, tangent and texture coordinates 16).
  Shadow passes read only the 12 bytes of the position.
- `cube()` gives a unit cube with normals, tangents and texture coordinates for tests.

## 2. Materials and textures

Materials are glTF 2.0's metallic-roughness model (ADR 0040). Every material lives in one
`MaterialTable`, every texture in one `TexturePool`:

```rust
use engine_assets::{DerivedDataCache, TextureUsage, import_texture};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable, TextureRef};
use engine_render::texture_pool::{TextureFormat, TextureLevels, TexturePool};

// Uploads at most 64 MB of texture data per frame.
let mut textures = TexturePool::new(&device, 64 << 20, IN_FLIGHT);
let mut materials = MaterialTable::new(IN_FLIGHT);

// A texture: decoded (PNG or JPEG), given its mip levels and block-compressed for its
// use, or loaded from the derived data cache when it was imported before.
let cache = DerivedDataCache::new("project/.ddc")?;
let (bark_bc7, _) = import_texture(&std::fs::read("bark_albedo.png")?, TextureUsage::Color,
                                   Some(&cache), Some(&jobs))?;
let bark = textures.add(&device, &mut allocator, TextureLevels {
    width: bark_bc7.width,
    height: bark_bc7.height,
    format: TextureFormat::Bc7Srgb, // bark_bc7.format
    levels: bark_bc7.levels,
})?;

let tree = materials.add(MaterialDesc {
    base_color_texture: Some(TextureRef::new(bark)),
    roughness: 0.8,
    ..MaterialDesc::default()
});
let red_plastic = materials.add(MaterialDesc::solid([0.8, 0.1, 0.1], 0.0, 0.4));

// Every frame, textures first, then materials, before the passes that draw:
textures.begin_frame(&mut fg, &mut allocator, &bindless, frame.number)?;
let mats = materials.begin_frame(&mut fg, &device, &mut allocator, &textures, frame.number)?;
```

- **Every glTF material parameter:**
  - base color (factor × sRGB texture, with alpha);
  - metallic and roughness (texture: blue and green);
  - normal map with its scale;
  - ambient occlusion (red) with its strength;
  - emission (factor × sRGB texture; above 1 for bright emitters, the target is HDR).
- **A missing texture is white:** the factor applies alone, and the shader skips the
  sample.
- **Textures are block-compressed** (ADR 0041): BC7 for color and data, BC5 for normal
  maps, a quarter of the memory of RGBA8.
  - Sponza's textures take 91 MB instead of 363, and its forward pass is 27% faster on an
    RTX 4060 Ti and 15% on a Radeon iGPU.
  - Compressing is slow: about 6 s for Sponza on 16 threads. The derived data cache keeps
    every result, keyed by the image's bytes, its use and the encoder's settings, so the
    next import of the same image takes milliseconds (13 ms for all of Sponza).
  - Keep one cache directory per project (or share it with the team). Deleting it only
    costs time.
  - `build_mips` and `TextureFormat::Rgba8Srgb`/`Rgba8Unorm` still work for uncompressed
    textures (procedural or editor-generated ones).
- **Pick the right `TextureUsage` when importing:**
  - `Color` for base color and emission: filtered in linear light, without dark fringes
    around cut-outs;
  - `ColorMasked { cutoff }` for base colors of alpha-tested materials: every level keeps
    the share of opaque texels, so foliage does not thin out in the distance;
  - `Data` for metallic-roughness and occlusion;
  - `Normal` for normal maps (vectors, renormalized).
- **Textures stream in.**
  - `add` creates the image at once; its data uploads within the frame budget, oldest
    first.
  - Until then, materials using it shade without it. A scene appears at once and its
    textures fill in over the next frames.
  - From packs, levels stream in one by one, smallest first: `add_empty`, `staging`,
    `upload_levels`. The texture is used as soon as its smallest levels are there and
    sharpens as larger ones arrive (see assets.md, "Streaming textures").
- **Alpha:**
  - `AlphaMode::Mask { cutoff }` discards texels below the cutoff, in the view and in
    shadows (leaves, fences).
  - `AlphaMode::Blend`: transparent, alpha is coverage (glass, water, visors). Drawn
    after the opaque surfaces, back to front; see section 12.
- **`double_sided: true`** draws both faces. Back faces are lit from their own side, and
  both faces cast shadows. Use it for leaves and cloth; closed meshes should stay
  single-sided (half the fragments).
- **Wrap modes** per texture reference: `TextureRef { texture, wrap: [Wrap::Repeat,
  Wrap::ClampToEdge] }`. Filtering is always trilinear with 16× anisotropy.
- **Edit and remove freely:**
  - `materials.set(id, desc)` applies next frame;
  - `materials.remove(id)`: draws that still use the id draw with the default material;
  - `textures.remove(id, frame.number, &bindless)`: materials using it shade without it
    from the next frame on, and its memory is freed once the GPU is done with it.
- **Specular antialiasing is built in:** highlights on small and curved geometry widen
  instead of flickering (Tokuyoshi and Kaplanyan's method, Filament's settings).
- glTF import: `engine_assets::import_gltf` lists each material's textures and their wrap
  modes, and `read_texture` loads a texture's encoded image. `examples/scene_bench.rs`
  turns a whole glTF scene into textures, materials and draws, decoding textures in
  parallel.
- Not yet: `KHR_texture_transform`, a second set of texture coordinates, vertex colors.

## 3. The draw list

```rust
use engine_render::forward::{DrawItem, ForwardView};

draws.clear();
for (transform, look) in scene_objects {
    draws.push(DrawItem {
        mesh: rock_mesh,
        material: look.material,
        world_from_object: transform,
    });
}
let passes = renderer.add_passes(
    &mut fg,
    &mut ring,
    &bindless,
    &geo,
    &mats,
    &draws,
    &ForwardView {
        camera,
        light,
        color: hdr,
        depth,
        clear: Some(clear),
        occlusion: None, // or Some(&occlusion): see section 5
    },
);
```

- **Keep objects with the same mesh next to each other in the list.**
  - Every run of consecutive items with the same mesh and the same kind of material
    (opaque or masked, single- or double-sided) is one batch, and one batch is one
    instanced draw. Items of one batch may use different materials of that kind.
  - 10,000 rocks followed by 500 trees make 2 batches. The same objects interleaved make
    up to 10,500.
  - Both render correctly, but the first is the fast one.
- **Batches are drawn front to back** (within each kind of material), so hidden fragments
  are rejected before shading: on Sponza that halves the forward pass. You do not need to
  sort anything yourself.
- **`renderer.set_depth_prepass(true)`** adds a depth-only pass first, so the forward pass
  shades each pixel exactly once (ADR 0042).
  - Turn it on for interiors and cities with expensive materials, especially on
    integrated GPUs: Sponza's textured frame gets 11% faster on a Radeon iGPU, a city of
    1,000 cubes 21%.
  - Leave it off for dense or open geometry. The prepass costs a second pass over every
    vertex: 100,000 cubes in the open take twice as long.
  - Compare both with `draw_bench -- --prepass` and `scene_bench -- --prepass`.
- **Reuse the `Vec`** (`clear()` and refill) so a frame makes no heap allocations.
- **Transforms are affine.** Translation, rotation and scale, including mirroring. The
  bottom row of the matrix is ignored. A mirrored object (negative determinant) keeps its
  front faces and its normal maps the right way round, as glTF requires.
- **The upload ring must fit the frame.** Size it with about 100 bytes per object plus a
  little per mesh.
  - `add_passes` returns `None` when the ring is full. It then declares nothing, and the
    frame has no 3D content.

### The GPU scene: instances that stay on the GPU (ADR 0064)

A draw list is rebuilt and uploaded every frame: 0.87 ms of CPU and 6 MiB per frame for
100,000 objects on the RTX 4060 Ti, even when nothing moves. A `GpuScene` keeps the
instances in GPU buffers across frames and uploads only what changed:

```rust
use engine_render::gpu_scene::{GpuScene, SceneInstance};

let mut scene = GpuScene::new(&device, &bindless, 4096, frames_in_flight)?;
let rock = scene.insert(SceneInstance { mesh: rock_mesh, material, world_from_object });
// Later, whenever something changes:
scene.set_transform(rock, &new_transform); // a bit-identical transform costs nothing
scene.teleport(rock, &spawn_point);        // no motion blur / TAA smear this frame
scene.remove(rock);

// Every frame, once, before the views that draw it:
let frame = scene
    .add_passes(&mut fg, &device, &mut allocator, &mut ring, &bindless, &geo, &mats, frame_number)?
    .expect("upload ring full");
renderer.add_scene_passes(&mut fg, &mut ring, &bindless, &geo, &mats, &frame, &view);
```

- **Cost follows changes.** 100,000 static objects: 2 µs of CPU and under 1 KiB uploaded
  per frame; 1 % moving: 46 µs and 95 KiB (RTX). On the Radeon iGPU it also saves the
  0.5 ms copy of every instance that motion vectors needed with a draw list.
- **Order does not matter.** Instances are batched by mesh and pipeline state whatever
  order you insert them in, and occlusion culling and motion vectors key off each
  instance's slot, not a position in a list.
- **Everything moving every frame** (particles, a stress test) is cheaper as a draw list:
  the scene does bookkeeping per moved instance (100,000 moving: 1.7 ms against 0.9 ms of
  CPU). A view draws either a list or a scene.
- **Motion vectors** come from the scene: give the view a `Temporal` for the camera and
  jitter as usual. New and teleported instances have no motion in their first frame.
- **Occlusion culling** works as with lists; size the `Occlusion` to at least
  `scene.slot_count()` so every instance is remembered.
- Removed slots are reused by the next insert; editing a material's kind (opaque, masked,
  blended, double-sided) or removing a material re-resolves all instances once.
- With the ECS, `engine_runtime::RenderExtract` keeps a scene in sync with every entity
  that has a `MeshRenderer` (see the ECS guide, section 10).
- Compare both paths: `cargo run -p engine_render --release --example gpu_scene_bench`.

## 4. Culling

- Culling is on by default:
  - every object's bounding box, from the mesh's box and its transform, is tested
    against the camera's view;
  - only objects at least partly in view reach the forward pass.
- Nothing is required from you. Meshes compute their box when created.
- Shadows still include objects outside the view, because they can cast shadows into it.
- `renderer.set_frustum_culling(false)` turns culling off, for comparisons or when
  debugging something that "disappears". Everything then counts as visible.
- To count what was drawn:
  - `add_passes` returns `ForwardPasses`, whose `commands` buffer holds one Vulkan draw
    command per detail level of each batch (ordered by kind of material, then front to
    back; `draw_commands` of them per list);
  - after the frame, `instance_count` in each command is the number of visible objects.
  - `crates/engine_render/tests/culling.rs` and `examples/draw_bench.rs` show how to read
    it back.

### Detail levels (LODs)

Far away, a mesh can be drawn with fewer triangles without anyone seeing the difference.
Give it detail levels and the renderer picks one per object, per frame (ADR 0053):

```rust
let levels = engine_assets::generate_lods(&mesh.positions, &mesh.indices, &LodSettings::default());
let lods: Vec<LodIndices> = levels.iter().map(|l| LodIndices { indices: &l.indices, error: l.error }).collect();
let id = geometry.add_mesh_with_lods(&mesh.as_data(), &lods)?;
```

- **Made from the mesh itself:** up to 3 levels, each about half the triangles of the
  previous, sharing the mesh's vertices (little extra memory). Small meshes (under a few
  hundred triangles) get none: they would not gain.
- **Chosen by how much it shows:** each level knows how far its surface strays from the
  full mesh; the renderer draws the coarsest level whose error stays within 1 pixel on
  screen at the object's distance and size. No per-asset distances to tune; it follows the
  resolution and field of view.
- `renderer.set_lod_threshold(2.0)` accepts 2 pixels (faster, softer silhouettes);
  `0.0` always draws full meshes (for comparisons).
- **What it saves:** a field of 16k-triangle spheres costs 6–9× less to shade on both
  reference GPUs (`draw_bench --sphere 64 --lods`). Scenes seen from inside with large
  meshes gain less (Sponza: about 10–15%).
- **Your own levels** (artist-made) work the same: pass their indices and their error.
- **Known limits:** levels switch without a cross-fade (at 1 pixel it rarely shows; TAA
  hides the rest); orthographic cameras draw full meshes; making levels at load takes
  time (155 ms for Sponza) until the derived data cache keeps them.
- **Streamed levels** (ADR 0076): a cooked pack stores each mesh by level; the engine
  loads the coarsest one first and the finer ones as objects come close enough to need
  them, within the GPU memory budget (`GeometryPool::add_streamed_mesh`,
  `engine_streaming::SceneStreamer`, see the assets guide). A level not loaded yet draws
  the finest one that is.

## 5. Occlusion culling: skipping what is hidden

In a city, an interior or a dense forest, most objects in view are hidden behind nearer
ones. Occlusion culling skips them (ADR 0035). Create one state per camera and pass it
every frame:

```rust
use engine_render::forward::Occlusion;

// Once per view: remembers which of up to 100,000 draw items were visible.
let mut occlusion = Occlusion::new(&device, &mut allocator, 100_000)?;

// Every frame:
let view = ForwardView { occlusion: Some(&occlusion), ..view };

// After a camera cut (teleport, new level):
occlusion.reset();
```

- **It never hides anything visible.**
  - The frame draws what was visible last frame, builds a depth pyramid from it, tests
    everything else against it, and draws what was missed.
  - A bad guess from last frame costs time, not pixels.
- **Keep the draw list in a stable order.** Items are known by their position in it.
  Reordering is allowed, but the frame after it culls less well.
- **Use it where things hide things, and the hidden things are expensive.**
  - In the benchmark's city (4 m walls around 10 m blocks), it drew 73 of 10,042 objects
    instead of 1,769.
  - It pays when what it skips costs more than its own passes (a depth pyramid and a second
    culling pass, 15-25 µs on an RTX 4060 Ti, 175-200 µs on a Radeon iGPU at 1080p, where
    reading the depth buffer alone takes about 100 µs of memory bandwidth). On the iGPU the
    city got 8-11% faster; on the RTX, with cubes that cost almost nothing to draw, it got
    slower. Detailed meshes and expensive materials change that.
  - In an open field nothing hides anything, and the extra passes are pure cost.
  - Turn it on per camera, and compare with `draw_bench` on the GPUs you target.
- The capacity is the number of draw items whose visibility is remembered. Items beyond
  it still render correctly, but are always tested in the late phase.
- Free the state with `unsafe { occlusion.destroy(&mut allocator) }` once the GPU is idle.

## 6. What it costs

`cargo run -p engine_render --release --example draw_bench` prints the costs for 1k, 10k
and 100k cubes, with the camera above the field, inside it and in a city of walls, and
culling off, frustum only, or frustum and occlusion:

- CPU time of building your list;
- CPU time of `add_passes` and of recording the frame;
- GPU time of the culling, depth pyramid, shadow and forward passes;
- how many objects were drawn.

`cargo run -p engine_render --release --example scene_bench -- test-assets/Sponza/Sponza.gltf`
measures a real glTF scene with its materials and textures, textured and with factors
only (fetch scenes with `tools/fetch-test-assets.ps1`; `--screenshot out.png` saves the
frame). `cargo run -p engine_assets --release --example texture_bench -- <scene>` measures
texture decoding and mip generation.

Results from the reference machine are in `docs/perf/history.md` and ADRs 0035, 0039 and
0040 and 0041. On Sponza at 1080p the textured forward pass takes 0.28 ms on an RTX 4060 Ti
and 6.9 ms on a Radeon iGPU. `scene_bench -- --uncompressed` compares with RGBA8
textures.

Rules of thumb:

- the CPU pays 64 bytes of upload and a few dozen nanoseconds per object;
- the GPU culls millions of objects per millisecond;
- the forward pass then costs what is on screen;
- the shadow pass costs every object that casts into a cascade (section 8).

## 7. Temporal antialiasing and upsampling (TAA / TAAU)

Temporal antialiasing removes the aliasing of edges, alpha-tested foliage, highlights
and shadow edges (ADR 0043):

- every frame the camera is moved by a sub-pixel offset (jitter);
- each pixel blends with where its surface was last frame, found through its motion
  vector.

Upsampling (TAAU) is the same resolve into a larger image: render at 720p, output
1080p. It works per view:

```rust
use engine_render::forward::Temporal;
use engine_upscale::{Taau, TaauInput};

// Once per view: remembers the transforms of up to 100,000 draw items.
let temporal = Temporal::new(&device, &mut allocator, 100_000)?;
let mut taau = Taau::new(&device, &bindless, frames_in_flight)?;
temporal.set_jitter_phases(Taau::jitter_phases(render_size, output_size));

// Every frame: `color` and `depth` at the render size.
let view = ForwardView { temporal: Some(&temporal), ..view };
let passes = renderer.add_passes(&mut fg, &mut ring, &bindless, &geo, &mats, &draws, &view)?;
let motion = passes.motion.expect("a temporal view reports motion");
let resolved = taau.add_pass(&mut fg, &device, &mut allocator, &bindless,
    TaauInput { color, depth, motion }, output_size, frame_number)?;
tonemap.add_pass(&mut fg, &bindless, resolved, backbuffer, exposure);

// After a camera cut (teleport, new level, switching cameras):
temporal.reset();
```

- **Keep the draw list in a stable order.**
  - Items are known by their position in it, as for occlusion culling.
  - An item whose position had nothing last frame (new, reordered, beyond the capacity)
    counts as still for one frame. It may smear briefly if it moved.
- **Give the transforms you draw.** With fixed-step physics, interpolate for rendering.
  Motion vectors report what was on screen, and a jump between two ticks looks like
  motion.
- **Call `reset()` on cuts.** Otherwise the first frame blends with a picture of
  somewhere else.
- **Changing the output size** recreates the history (and restarts accumulation).
  Changing the render size only needs new jitter phases.
- **Motion vectors are yours too.** `MotionOutput::velocity` (RG16F, the render size) is
  the texture-coordinate delta current − previous, without jitter. The surface at `uv`
  was at `uv − motion` last frame. Pixels without geometry carry the camera's motion. Use
  it for motion blur or your own temporal effects.
- **History filter:** bilinear by default (the fastest). For sharper images in motion,
  `taau.set_history_filter(HistoryFilter::CatmullRom)`: 10-15% more resolve time (about
  1 dB better on a panning camera).
- **Texture LOD bias while upsampling:** textures are sampled for the render size unless
  you bias them towards the output size:
  `materials.set_texture_lod_bias(&bindless, engine_upscale::texture_lod_bias(render_size, output_size))?`
  (log2(render / output): −0.58 from 720p to 1080p, −1 from half the size). It applies to
  every material texture of the table, so to every view drawing with it; set it when the
  render or output size changes. It costs nothing per pixel (it lives in the samplers). On a finely textured ground upsampled from half the
  size it gains 0.1-0.3 dB with TAAU and FSR; AMD's recommendation of one level more
  (`Fsr::texture_lod_bias`) was worse there (ADR 0060). Leave it at 0 for plain TAA.
- **Known limits for now:**
  - transparent surfaces lower the history's weight where they cover (section 12), but
    there is no mask for animated textures yet;
  - no sharpening in TAAU (FSR has one, below).

What it costs (ref-1, 1080p):

- **Motion vectors:** +8-23% on the forward pass (Sponza, FlightHelmet). Measure with
  `scene_bench -- --motion`.
- **The resolve:**
  - RTX 4060 Ti: 0.14 ms for TAA, 0.12 ms from 720p (bilinear history);
  - Radeon iGPU: 3.9 ms either way (4.5 ms with the Catmull-Rom history).
  - Pass `-- --catmull-rom` to the benchmark for the other filter.
  - Measure with `cargo run -p engine_upscale --release --example taau_bench`.
- **Upsampling from 720p** is cheaper than native 1080p with TAA on both GPUs:
  - RTX: 0.19 ms against 0.30 ms for forward + resolve in that benchmark;
  - Radeon: 5.8 ms against 7.7 ms.

### AMD FSR 3.1 instead of TAAU

`engine_fsr` runs AMD's FidelityFX Super Resolution 3.1 upscaler on the same inputs (ADR
0060). It is compiled into the engine from the FidelityFX SDK's source: there is no DLL to
ship or install, and it works on any GPU the engine runs on.

```rust
use engine_fsr::{Fsr, FsrCamera, FsrInput, QualityMode};

// Once per view.
let mut fsr = Fsr::new(&device, frames_in_flight);
let render_size = QualityMode::Quality.render_size(output_size); // 1080p → 1280×720
temporal.set_jitter_phases(Fsr::jitter_phases(render_size, output_size));
materials.set_texture_lod_bias(&bindless, engine_upscale::texture_lod_bias(render_size, output_size))?;
fsr.set_sharpness(Some(0.3)); // optional: RCAS after upscaling, 0-1

// Every frame: `color` and `depth` at `render_size`, as for TAAU.
let passes = renderer.add_passes(&mut fg, &mut ring, &bindless, &geo, &mats, &draws, &view)?;
let upscaled = fsr.add_pass(&mut fg, FsrInput {
    color,
    depth,
    motion: passes.motion.expect("a temporal view reports motion"),
    camera: FsrCamera { near, far, fov_y, meters_per_unit: 1.0 },
    frame_time_ms,
}, output_size, frame_number)?;
tonemap.add_pass(&mut fg, &bindless, upscaled, backbuffer, exposure);
```

- **Quality modes:** `NativeAa` (1.0×, antialiasing only), `Quality` (1.5× per axis),
  `Balanced` (1.7×), `Performance` (2.0×), `UltraPerformance` (3.0×).
- **Give it the camera you drew with:** near and far planes and the vertical field of
  view (radians) of the projection, and the real frame time in milliseconds. FSR
  linearizes depth and scales its motion thresholds with them.
- **Cuts:** `temporal.reset()` as for TAAU (or `fsr.reset()`).
- **Changing the output size**, or a render size larger than any before, creates a new
  FSR context: 11-14 ms on the RTX 4060 Ti, 55-70 ms on the Radeon iGPU. Do it in menus,
  not every frame. A smaller render size (dynamic resolution) costs nothing.
- **Exposure:** give FSR the exposure the tone mapper applies (`FsrInput::exposure`, a
  1 × 1 `R32_SFLOAT` texture; the frame renderer does it): +0.3 to +0.5 dB against FSR's
  own measurement (ADR 0072). `None` lets FSR measure its own.
- **Not available:** frame generation (planned, ADR 0060); FSR 4 (AMD ships it for DX12
  only).

When to choose it (ref-1, `cargo run -p engine_fsr --release --example upscale_bench`):

| 1080p from 720p | TAAU | FSR 3.1 |
|---|---:|---:|
| RTX 4060 Ti, GPU time | 0.12 ms | 0.53 ms |
| Radeon iGPU, GPU time | 3.9 ms | 10.9 ms |
| image, camera still / moving (PSNR against 2×2 supersampling) | 42.6 / 36.9 dB | 43.7 / 38.6 dB |

- FSR's image is better, about 1 dB with the camera still and 1.6-2.1 dB in motion, at 3-5×
  TAAU's cost on a discrete GPU. On the integrated Radeon it costs more than rendering at
  the output size: keep TAAU there.
- FSR pays when the rendering it saves (1080p against 720p is 2.25× the pixels) costs
  more than its 0.5 ms. In a light scene it does not; in a heavy one it does.
- FSR holds about 130 MiB of GPU memory at 1440p (its history and intermediate images).

### NVIDIA DLSS instead of TAAU

`engine_dlss` runs NVIDIA's DLSS Super Resolution (and DLAA, the same network at the
output size) through NVIDIA Streamline (ADR 0063). It runs only on NVIDIA RTX GPUs, and
only with NVIDIA's DLLs, which **the engine does not ship**: the player, or you, put them
next to the game.

**The DLLs:** [upscalers.md](upscalers.md) lists them, where they go, how to check them
(`cargo run -p engine_dlss --example dlss_probe`), and what NVIDIA's license asks of a
game that ships them.

**Setup.** Streamline must start before the Vulkan device exists, so DLSS takes part in
creating it:

```rust
use engine_dlss::{Dlss, Streamline, StreamlineDesc};
use engine_rhi::{Device, Instance};
use engine_upscale::Quality;

// 1. Before the instance: an error means "no DLSS" (missing or unsigned DLLs, no NVIDIA
//    driver). Hide the option; nothing else changes.
let streamline = Streamline::load(&StreamlineDesc {
    project_id: String::from("your game's GUID"),
    ..StreamlineDesc::default()
}).ok();
let instance = Instance::with_extensions(&instance_desc,
    &streamline.as_ref().map(Streamline::instance_extensions).unwrap_or_default())?;
let adapter = instance.select_adapter(&preference)?;
// 2. Does this GPU run DLSS? (An RTX GPU with a recent driver.)
let streamline = streamline.filter(|s| s.check_adapter(&instance, &adapter).is_ok());
let device = Device::with_extensions(&instance, &adapter,
    &streamline.as_ref().map(Streamline::device_extensions).unwrap_or_default())?;
// 3. Hand over the device, and let Streamline see your presents.
if let Some(streamline) = &streamline {
    streamline.attach(&device)?;
    unsafe { swapchain.set_present_function(streamline.present_function()) };
}

// Once per view.
let mut dlss = Dlss::new(streamline.as_ref().unwrap(), frames_in_flight)?;
let render_size = dlss.render_size(Quality::Quality, output_size)?; // 1080p → 1280×720
temporal.set_jitter_phases(Dlss::jitter_phases(render_size, output_size));
materials.set_texture_lod_bias(&bindless, Dlss::texture_lod_bias(render_size, output_size))?;

// Every frame: as for FSR.
let upscaled = dlss.add_pass(&mut fg, UpscaleInput {
    color, depth, motion: passes.motion.expect("a temporal view reports motion"),
    camera: UpscaleCamera { near, far, fov_y }, frame_time_ms,
}, output_size, frame_number)?;
tonemap.add_pass(&mut fg, &bindless, upscaled, backbuffer, exposure);

// At shutdown, before the device: free the views' DLSS, then drop Streamline.
unsafe { dlss.destroy() };
drop(dlss);
drop(streamline);
```

- **Modes** follow from the sizes you render and output: DLAA at the output size, else
  Quality (1.5× per axis), Balanced (1.72×), Performance (2×) or Ultra Performance (3×).
  Take render sizes from `dlss.render_size` (NVIDIA's numbers); `dlss.render_sizes`
  gives the range each mode accepts for dynamic resolution.
- **Presets** (NVIDIA's networks): `dlss.set_preset(DlssPreset::K)` and so on. The default
  lets NVIDIA choose: K for DLAA, Quality and Balanced, M for Performance, L for Ultra
  Performance. On the RTX 4060 Ti, K is the cheapest (0.87 ms at 1080p from 720p); M
  costs 1.3-3.5× K, L 1.6-4.4×.
- **Texture LOD bias:** NVIDIA's recommendation, `Dlss::texture_lod_bias` (one level
  sharper than `engine_upscale::texture_lod_bias`), measured best with DLSS.
- **Cuts:** `temporal.reset()` (or `dlss.reset()`).
- **Changing the output size, the mode or the preset** creates a new DLSS feature: 26 ms
  on the first frame. Do it in menus.
- **Startup:** loading Streamline takes about 0.7 s and attaching the device 0.4 s. Load
  it only when the player enabled DLSS, or early, beside other startup work.
- **Exposure:** `UpscaleInput::exposure` gives DLSS the exposure the tone mapper applies
  (+0.1 to +0.8 dB against DLSS's own measurement, ADR 0072); `None` lets DLSS measure.
- **Not passed to DLSS:** the reactive mask of transparent surfaces (NVIDIA's current
  networks take none).
- **Not available yet:** DLSS frame generation and Ray Reconstruction (ADR 0063).

What it costs and gives (ref-1, RTX 4060 Ti, `cargo run -p engine_dlss --release
--example dlss_bench`, FSR's scene):

| 1080p from | TAAU | FSR 3.1 | DLSS |
|---|---:|---:|---:|
| 1080p (DLAA), GPU time | 0.15 ms | 0.79 ms | 0.89 ms |
| 720p, GPU time | 0.12 ms | 0.53 ms | 0.87 ms |
| 540p, GPU time | 0.12 ms | 0.44 ms | 1.12 ms (0.86 with preset K) |
| 720p, image still / moving (PSNR against 2×2 supersampling) | 42.6 / 36.8 dB | 43.5 / 38.9 dB | 46.0 / 39.8 dB |
| 540p, image still / moving | 40.4 / 35.1 dB | 41.6 / 37.0 dB | 44.3 / 41.5 dB |
| GPU memory | 32 MiB at 1080p (two history images) | 134 MiB at 1440p | 85 MiB at 1080p (124 with preset M), 149 MiB at 1440p |

- DLSS gives the best image by a clear margin: from 540p it beats FSR from 720p.
- It costs the most GPU time: it pays where the rendering it saves costs more than about
  0.9 ms at 1080p, that is, in heavy scenes, not in this light one.

### Choosing an upscaler: one interface

`Taau`, `Fsr` and `Dlss` all implement `engine_upscale::Upscaler`, so a view can hold
whichever the player chose:

```rust
use engine_upscale::{Quality, UpscaleContext, UpscaleInput, Upscaler, UpscalerKind};

let mut upscaler: Box<dyn Upscaler> = match settings.upscaler {
    UpscalerKind::Taau => Box::new(Taau::new(&device, &bindless, frames_in_flight)?),
    UpscalerKind::Fsr => Box::new(Fsr::new(&device, frames_in_flight)),
    UpscalerKind::Dlss => Box::new(Dlss::new(&streamline, frames_in_flight)?),
};
let render_size = upscaler.render_size(settings.quality, output_size);
temporal.set_jitter_phases(upscaler.jitter_phases(render_size, output_size));
materials.set_texture_lod_bias(&bindless, upscaler.texture_lod_bias(render_size, output_size))?;

// Every frame:
let upscaled = upscaler.add_pass(&mut fg,
    UpscaleContext { device: &device, allocator: &mut allocator, bindless: &bindless },
    input, output_size, frame_number)?;
```

`UpscalerKind::name()` gives the menu text ("TAAU", "FSR 3.1", "DLSS"). Offer DLSS only
when `Streamline::load`, `check_adapter` and `attach` succeeded.

### Choosing antialiasing: every mode, what it costs, what it gives

The engine implements every common antialiasing and scaling method (ADR 0075) so that
**you** decide what your players may choose. A view's `ViewSettings::antialiasing` is made
of three independent parts:

```rust
use engine_frame::{Antialiasing, Msaa, PostAntialiasing, SmaaQuality, Upscale, UpscalerKind};

let aa = Antialiasing {
    samples: Msaa::X4,                       // raster samples: X1, X2, X4, X8 (MSAA)
    upscale: Upscale::Bilinear,              // render size → output (below)
    post: PostAntialiasing::Smaa(SmaaQuality::High), // after tone mapping, before UI
};
```

- **`samples`** — MSAA. Geometry edges get 2/4/8 coverage samples; shading still runs
  once per pixel. Masked (alpha-tested) materials turn their alpha into coverage (alpha
  to coverage). Views with MSAA always draw the depth prepass. Ambient occlusion, contact
  shadows, reflections, subsurface scattering, the atmosphere and fog all work with it
  (each is resolved so it keeps its edges; AO and contact shadows stay one value per
  pixel, which you may see as a faint darkening on the far side of a silhouette).
- **`upscale`** — how the render size (`quality`, `render_scale` or `render_size`)
  reaches the output:
  - `Bilinear`: the soft stretch (nothing at the output size: plain rendering);
  - `Nearest`: blocky, no blur;
  - `Integer`: pixel art. Set `ViewSettings::render_size = Some((320, 180))`; the image is
    shown at the largest whole multiple that fits the output (×6 at 1080p, ×8 at 1440p),
    centered, black around. Every scene pixel is a square of screen pixels;
  - `Fsr1`: AMD FSR 1, edge-adaptive upscaling plus sharpening (`ViewSettings::sharpness`,
    in stops: 0 the strongest, AMD's default 0.2). At the output size it only sharpens;
  - `Temporal(UpscalerKind::Taau | Fsr | Dlss)`: TAA/TAAU, FSR 3.1, DLSS (sections above);
  - `SmaaT2x`: SMAA's two-frame temporal mode (needs `post: Smaa(..)` and the output size).
- **`post`** — a filter on the tone-mapped image: `Fxaa`, `Mlaa`, `Smaa(Low | Medium |
  High | Ultra)`. With `Msaa::X2` SMAA runs on each sample (SMAA S2x); with `SmaaT2x`
  too, that is SMAA 4x.

Named constants cover the usual modes: `Antialiasing::OFF`, `TAAU`, `FSR`, `DLSS`,
`FXAA`, `MLAA`, `SMAA_1X`, `SMAA_T2X`, `SMAA_S2X`, `SMAA_4X`, `Antialiasing::msaa(Msaa::X4)`,
`Antialiasing::spatial(Upscale::Fsr1)`, and `.with_samples(..)` / `.with_post(..)` to
combine.

**Building a menu.** Offer what *your* game wants, filtered by what the player's GPU runs:

```rust
let support = frame.antialiasing_support();   // MSAA sample counts, available upscalers
let mut menu = Vec::new();
for (label, aa) in my_whitelist() {
    let candidate = ViewSettings { antialiasing: aa, ..settings };
    if frame.check(&candidate, output_size).is_ok() {
        menu.push(label);   // `check`'s error says why a mode cannot run
    }
}
```

`prepare` refuses the same combinations with `FrameError::Antialiasing`: SMAA T2x without
SMAA or below the output size, MSAA under FSR 3.1 or DLSS (both want aliased input), a
sample count the GPU lacks. A temporal upscaler that is not installed (DLSS without
Streamline) is not an error: the view falls back to TAAU. A mode a view does not use
costs nothing: no passes, no pipelines (they are made the first time a view asks), no
shader registers.

**What each mode costs and gives** (ref-1, Sponza 1920 × 1080 with every feature on:
atmosphere, fog, GTAO High, contact shadows, local lights with shadows, reflection
probes, SSR, bloom). Passes: the GPU time of the whole frame; FLIP and PSNR against a
4 × 4 supersampled frame (lower FLIP, higher PSNR are better), still and with the camera
strafing slowly; flicker: how much the image jumps from frame to frame beyond what the
supersampled image does, with every feature on and on a plain sun-and-sky Sponza (lower
is better).

**RTX 4060 Ti**

| mode | frame ms | vs off ms | own passes ms | still FLIP | still PSNR dB | moving FLIP | moving PSNR dB | flicker % | flicker %, plain |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| off | 2.18 | +0.00 | – | 0.0240 | 37.39 | 0.0241 | 37.38 | 1.85 | 3.36 |
| FXAA | 2.21 | +0.03 | 0.05 | 0.0255 | 38.34 | 0.0255 | 38.30 | 1.45 | 2.21 |
| SMAA 1x medium | 2.39 | +0.21 | 0.11 | 0.0245 | 38.49 | 0.0245 | 38.43 | 1.67 | 2.80 |
| MLAA | 2.40 | +0.22 | 0.11 | 0.0246 | 38.37 | 0.0246 | 38.36 | 1.68 | 2.80 |
| SMAA 1x low | 2.29 | +0.11 | 0.10 | 0.0243 | 38.41 | 0.0244 | 38.37 | 1.74 | 3.12 |
| SMAA 1x high | 2.42 | +0.24 | 0.14 | 0.0244 | 38.55 | 0.0243 | 38.54 | 1.69 | 2.87 |
| SMAA 1x ultra | 2.43 | +0.26 | 0.16 | 0.0248 | 38.53 | 0.0247 | 38.51 | 1.45 | 2.43 |
| SMAA T2x | 2.54 | +0.36 | 0.21 | 0.0237 | 39.03 | 0.0255 | 38.43 | 1.00 | 1.70 |
| SMAA S2x | 3.30 | +1.12 | 0.59 | 0.0246 | 38.90 | 0.0245 | 38.92 | 1.47 | 2.08 |
| SMAA 4x | 3.45 | +1.27 | 0.75 | 0.0243 | 39.48 | 0.0259 | 38.90 | 0.90 | 1.41 |
| MSAA 2× | 2.90 | +0.72 | 0.20 | 0.0240 | 38.52 | 0.0239 | 38.56 | 1.60 | 2.52 |
| MSAA 4× | 3.33 | +1.16 | 0.29 | 0.0239 | 39.33 | 0.0239 | 39.35 | 1.35 | 1.82 |
| MSAA 8× | 4.30 | +2.12 | 0.43 | 0.0243 | 39.29 | 0.0242 | 39.32 | 1.33 | 1.63 |
| MSAA 4× + SMAA | 3.34 | +1.16 | 0.42 | 0.0244 | 39.23 | 0.0244 | 39.15 | 1.37 | 1.70 |
| MSAA 4× + TAA | 3.70 | +1.52 | 0.58 | 0.0276 | 38.73 | 0.0332 | 36.17 | 0.27 | 0.15 |
| TAA | 2.48 | +0.30 | 0.15 | 0.0266 | 38.87 | 0.0321 | 36.43 | 0.26 | 0.18 |
| TAAU 67 % | 1.52 | -0.65 | 0.13 | 0.0328 | 36.92 | 0.0385 | 34.99 | 0.27 | 0.15 |
| TAAU 50 % | 1.16 | -1.02 | 0.13 | 0.0438 | 33.88 | 0.0492 | 32.71 | 0.29 | 0.13 |
| FSR 3.1 Native AA | 3.06 | +0.88 | 0.83 | 0.0269 | 39.85 | 0.0241 | 38.90 | 0.60 | 1.14 |
| FSR 3.1 Quality (67 %) | 1.95 | -0.23 | 0.57 | 0.0335 | 37.64 | 0.0301 | 37.44 | 0.50 | 0.72 |
| DLAA | 3.22 | +1.04 | 0.96 | 0.0190 | 42.77 | 0.0242 | 40.42 | 0.50 | 0.79 |
| DLSS Quality (67 %) | 2.36 | +0.08 | 0.95 | 0.0222 | 41.03 | 0.0263 | 39.12 | 0.50 | 0.74 |
| bilinear 67 % | 1.29 | -0.99 | – | 0.0370 | 34.50 | 0.0365 | 34.58 | 1.17 | 1.78 |
| bilinear 50 % | 0.90 | -1.37 | – | 0.0512 | 31.97 | 0.0506 | 31.99 | 1.28 | 1.80 |
| nearest 67 % | 1.29 | -0.98 | – | 0.0418 | 32.65 | 0.0413 | 32.66 | 2.25 | 4.30 |
| nearest 50 % | 0.90 | -1.37 | – | 0.0489 | 31.35 | 0.0483 | 31.43 | 2.00 | 3.13 |
| integer 50 % | 0.90 | -1.37 | – | 0.0489 | 31.35 | 0.0483 | 31.43 | 2.00 | 3.13 |
| FSR 1 RCAS only (100 %) | 2.33 | +0.06 | 0.06 | 0.0405 | 30.90 | 0.0423 | 30.53 | 17.11 | 24.09 |
| FSR 1 67 % | 1.54 | -0.73 | 0.20 | 0.0364 | 33.87 | 0.0368 | 33.69 | 4.86 | 7.82 |
| FSR 1 50 % | 1.17 | -1.11 | 0.20 | 0.0470 | 31.97 | 0.0464 | 32.07 | 3.45 | 5.06 |
| FSR 1 67 % + MSAA 4× | 2.03 | -0.25 | 0.30 | 0.0358 | 34.65 | 0.0354 | 34.69 | 4.44 | 6.39 |
| FSR 1 67 % + SMAA | 1.68 | -0.59 | 0.34 | 0.0362 | 34.16 | 0.0364 | 33.98 | 4.82 | 7.71 |

**Radeon iGPU (2 CU)**

| mode | frame ms | vs off ms | own passes ms | still FLIP | still PSNR dB | moving FLIP | moving PSNR dB | flicker % | flicker %, plain |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| off | 58.17 | +0.00 | – | 0.0233 | 37.64 | 0.0234 | 37.61 | 1.91 | 3.48 |
| FXAA | 59.68 | +1.51 | 1.42 | 0.0251 | 38.62 | 0.0251 | 38.55 | 1.50 | 2.31 |
| SMAA 1x medium | 60.60 | +2.42 | 2.19 | 0.0240 | 38.79 | 0.0241 | 38.70 | 1.73 | 2.90 |
| MLAA | 60.33 | +2.16 | 2.18 | 0.0242 | 38.68 | 0.0242 | 38.59 | 1.74 | 2.89 |
| SMAA 1x low | 60.10 | +1.93 | 1.95 | 0.0238 | 38.71 | 0.0239 | 38.64 | 1.80 | 3.22 |
| SMAA 1x high | 60.61 | +2.44 | 2.49 | 0.0239 | 38.86 | 0.0239 | 38.82 | 1.76 | 2.99 |
| SMAA 1x ultra | 61.44 | +3.27 | 3.25 | 0.0244 | 38.84 | 0.0244 | 38.79 | 1.52 | 2.58 |
| SMAA T2x | 61.25 | +3.08 | 3.45 | 0.0233 | 39.44 | 0.0252 | 38.71 | 1.06 | 1.82 |
| SMAA S2x | 77.29 | +19.12 | 8.22 | 0.0241 | 39.29 | 0.0240 | 39.26 | 1.53 | 2.19 |
| SMAA 4x | 78.67 | +20.50 | 10.01 | 0.0233 | 39.95 | 0.0251 | 39.27 | 0.96 | 1.51 |
| MSAA 2× | 70.32 | +12.15 | 2.18 | 0.0233 | 38.82 | 0.0232 | 38.83 | 1.67 | 2.67 |
| MSAA 4× | 78.73 | +20.56 | 2.96 | 0.0231 | 39.71 | 0.0231 | 39.69 | 1.42 | 1.98 |
| MSAA 8× | 94.31 | +36.14 | 5.31 | 0.0235 | 39.67 | 0.0235 | 39.66 | 1.40 | 1.78 |
| MSAA 4× + SMAA | 81.38 | +23.21 | 5.38 | 0.0239 | 39.62 | 0.0240 | 39.49 | 1.44 | 1.84 |
| MSAA 4× + TAA | 83.72 | +25.54 | 8.23 | 0.0275 | 39.00 | 0.0329 | 36.30 | 0.26 | 0.15 |
| TAA | 61.61 | +3.43 | 3.84 | 0.0264 | 39.17 | 0.0319 | 36.55 | 0.26 | 0.17 |
| TAAU 67 % | 34.47 | -23.71 | 3.86 | 0.0328 | 36.87 | 0.0386 | 34.95 | 0.27 | 0.14 |
| TAAU 50 % | 25.23 | -32.94 | 3.85 | 0.0439 | 33.73 | 0.0496 | 32.63 | 0.29 | 0.12 |
| FSR 3.1 Native AA | 77.61 | +19.44 | 19.86 | 0.0243 | 40.42 | 0.0236 | 39.22 | 0.61 | 1.16 |
| FSR 3.1 Quality (67 %) | 43.06 | -15.11 | 12.37 | 0.0327 | 37.53 | 0.0297 | 37.34 | 0.50 | 0.69 |
| bilinear 67 % | 29.59 | -28.59 | – | 0.0356 | 34.77 | 0.0353 | 34.82 | 1.21 | 1.90 |
| bilinear 50 % | 19.71 | -38.47 | – | 0.0500 | 32.09 | 0.0493 | 32.10 | 1.33 | 1.91 |
| nearest 67 % | 29.62 | -28.56 | – | 0.0412 | 32.75 | 0.0408 | 32.75 | 2.51 | 5.18 |
| nearest 50 % | 19.72 | -38.45 | – | 0.0476 | 31.44 | 0.0470 | 31.53 | 2.14 | 3.66 |
| integer 50 % | 19.68 | -38.49 | – | 0.0476 | 31.44 | 0.0470 | 31.53 | 2.14 | 3.66 |
| FSR 1 RCAS only (100 %) | 59.31 | +1.14 | 1.22 | 0.0421 | 30.33 | 0.0433 | 30.02 | 19.58 | 27.32 |
| FSR 1 67 % | 35.16 | -23.01 | 4.89 | 0.0364 | 33.90 | 0.0368 | 33.74 | 6.07 | 10.31 |
| FSR 1 50 % | 25.52 | -32.65 | 4.89 | 0.0465 | 32.05 | 0.0459 | 32.14 | 3.84 | 6.16 |
| FSR 1 67 % + MSAA 4× | 46.12 | -12.05 | 6.11 | 0.0358 | 34.68 | 0.0352 | 34.79 | 5.64 | 8.92 |
| FSR 1 67 % + SMAA | 37.79 | -20.39 | 7.43 | 0.0362 | 34.20 | 0.0365 | 34.04 | 6.00 | 10.14 |

(Rows from runs on one afternoon with a quiet CPU; "vs off" against the "off" of the same
run, `own passes` the mode's own passes: MSAA's other cost is in the scene passes. The
spatial rows on the RTX and DLSS Quality are from a second run. Measure your own scenes
with `cargo run -p engine_golden --release --example aa_bench`.)

How to read it:

- **The best image when it is available:** DLAA / DLSS (RTX only, the player's DLLs):
  the best still and moving image and almost no flicker; DLSS Quality costs about what no
  antialiasing at 100 % costs.
- **Sharp and stable without temporal blur:** MSAA 4× (RTX +1.2 ms, Radeon +20.6 ms:
  +53 % / +35 % of the frame): the best moving image of all but DLAA (its edges do not
  blur when the camera moves), flicker almost halved against no antialiasing. MSAA 8× costs
  twice as much and gives nothing more on Sponza (its aliasing is mostly inside surfaces,
  where samples do not help). MSAA 2× is half the gain for 60 % of the cost.
- **The least flicker:** the temporal modes (TAA, TAAU, FSR 3.1, DLSS): 0.1–0.8 % of
  pixels against 3.4 % without antialiasing. The price is softness in motion (TAA's moving
  FLIP 0.032 against 0.024 without antialiasing) and ghosting; MSAA under TAA adds cost
  and nothing visible here.
- **Cheap and safe:** SMAA 1x High: +0.14 ms on the RTX, +2.5 ms on the Radeon, +1.2 dB,
  edges smoothed without blurring textures. FXAA is cheaper still (0.05 / 1.4 ms) and
  removes a third of the flicker but blurs textures (the worst still FLIP of the filters).
  MLAA lands between SMAA Low and Medium; SMAA Ultra is not better than High on Sponza.
- **SMAA's composite modes:** T2x (+0.2 / 3.5 ms) cuts SMAA 1x's flicker by 40 % with only two
  frames of history (a little softness in motion); S2x costs MSAA 2×'s scene cost plus a
  second SMAA; 4x is the steadiest of the family, with the softness of a two-frame history.
- **Integrated GPUs:** render size decides: TAAU from 67 % (34 ms) costs 60 % of plain
  1080p (58 ms) with less flicker; post filters cost 1.4–3.3 ms; MSAA 4× costs a third of
  the frame. The engine's integrated default (TAAU from 67 %) stays.
- **Scaling for looks:** nearest and integer scaling are free (in the tone mapper) and
  keep pixels square; integer scaling is the pixel-art mode (identical to nearest at a
  whole factor, letterboxed otherwise). Bilinear is the softest and flickers least of
  the spatial modes.
- **FSR 1** (0.2 / 4.9 ms) sharpens: against a smooth supersampled reference it scores
  below bilinear in PSNR, slightly better in FLIP, and its sharpening amplifies aliasing
  (flicker 4.9 % at 67 %, 17 % with RCAS alone at 100 %). AMD asks for an antialiased
  input: give it MSAA (less flicker, better PSNR) or raise `sharpness` (in stops: 1 is
  half as strong); for upscaling, TAAU from the same size is both sharper and steadier
  here.

## 8. Shadows of the sun: cascaded shadow maps

The directional light casts through up to four cascades (ADR 0044):

- the view is cut along its depth into slices, finest near the camera;
- each slice gets its own square shadow map, a tile of one atlas.

A 15 cm pole 4 m away keeps a crisp shadow, and a building 100 m away still casts one.

```rust
use engine_render::forward::ShadowSettings;

let renderer = ForwardRenderer::new(&device, &bindless, HDR_FORMAT, depth_format,
    ShadowSettings {
        cascades: 4,        // 1–4
        resolution: 1024,   // texels per cascade; the atlas holds 2 × 2
        distance: 150.0,    // how far from the camera shadows reach (world units)
        ..ShadowSettings::default()
    })?;
```

- **Nothing to do per frame.** Cascades follow the camera and the light.
  - Their maps move in whole texels and keep their size when the camera turns, so shadow
    edges do not crawl.
  - Shadows never reach beyond your scene's bounds or the camera's far plane. Shorter
    `distance` means sharper cascades.
- **Only objects that can cast into a cascade are drawn into it.** The GPU culls casters
  per cascade, including objects outside the camera's view whose shadows fall into it.
- **Cascade edges dither** over a band (`blend`, 10% of a cascade by default):
  - with TAA (section 7) the dither resolves into a smooth transition;
  - without it, it is a fine, still pattern;
  - `blend: 0.0` gives hard seams.
- **Acne and gaps:** receivers are pushed along their normal by `normal_bias` texels of
  their cascade (1.0). Raise it if lit surfaces at grazing angles to the sun show dark
  stripes; lower it if shadows detach from contact points.

What it costs (ref-1, 1080p; `scene_bench -- --cascades N --shadow-res R`, `draw_bench`):

| Sponza (an interior), shadow / forward µs | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| 2 cascades of 1024² | 76 / 303 | 1,197 / 7,519 |
| 4 cascades of 1024² (default) | 159 / 317 | 2,292 / 7,778 |
| 4 cascades of 2048² | 353 / 333 | 4,103 / 8,057 |

- **Open worlds with many objects** gain: at 100,000 cubes the shadow pass fell from
  263 µs to 18–83 µs on the RTX.
- **Small interiors** pay a pass over their geometry per cascade. The whole of Sponza
  sits in every cascade.
- **On integrated GPUs, start with 2 cascades.** Most of the sharpness is in the first
  ones.

### Contact shadows (ADR 0071)

The cascades cannot resolve what is smaller than their texels: a box resting on the floor,
a hand on a table. Contact shadows find it in the depth buffer (Bend Studio's screen-space
shadows):

```rust
renderer.set_contact_shadows(&device, &bindless, Some(ContactShadowSettings::default()))?;
```

- They need the depth before shading: the depth prepass runs while they are on.
- Off by default: Sponza at 1080p costs 0.12 ms on an RTX 4060 Ti and 2.6 ms on the
  Radeon iGPU (plus ~10 % of its forward pass).
- They work best with the sun behind or beside objects; a ray that passes behind a thick
  object's front face is not seen (the shadow map covers it).
- `surface_thickness` (0.01) is how thick a pixel is assumed to be: larger darkens more and
  risks false shadows behind thin objects.

## 9. Point and spot lights

Any number of local lights (ADR 0045), up to 65,535 per view:

```rust
use engine_render::forward::{LocalLight, Spot};

let lights = [
    // A lamp: color × intensity, and the distance where its light ends.
    LocalLight { position: Vec3::new(2.0, 3.0, -4.0), range: 8.0,
                 color: Vec3::new(1.0, 0.8, 0.6) * 20.0, spot: None, shadow: false },
    // A spot pointing down, full between 0.3 rad of its axis, fading out by 0.5.
    LocalLight { position: Vec3::new(0.0, 5.0, 0.0), range: 12.0, color: Vec3::splat(40.0),
                 spot: Some(Spot { direction: Vec3::NEG_Y, inner: 0.3, outer: 0.5 }),
                 shadow: false },
];
let view = ForwardView { lights: &lights, ..view };
```

- **`range` is the knob that matters for speed.** A light costs only the pixels within
  its range. Its falloff is inverse-square, windowed to reach zero exactly there. Set it
  to where the light stops mattering, not larger.
- **Nothing to do per frame.** The renderer culls lights outside the view, sorts the
  rest by depth, and has each pixel shade only the lights in both its screen tile and its
  depth slice.
- **A frame without lights costs nothing extra:** the shaders are compiled without the
  light loop then.
- **Debugging:** `renderer.set_light_culling(false)` makes every pixel evaluate every
  light that reaches the view (the reference; slow).
- **Shadows:** see below. **Not yet:** physical light units.

What it costs (ref-1, Sponza at 1080p, lights spread over the scene with ranges of 3–8%
of its size; `scene_bench -- --lights N`):

| lights | RTX 4060 Ti forward | Radeon iGPU forward |
|---:|---:|---:|
| 0 | 0.32 ms | 7.7 ms |
| 256 | 0.47 ms | 14.1 ms |
| 1,024 | 0.85 ms | 25.9 ms |
| 4,096 | 2.45 ms | 72.6 ms |

- **The CPU pays about 55 ns per light** (culling, sorting, binning).
- **The GPU pays most for the lights that actually overlap each pixel.** Many small
  lights are cheaper than a few huge ones.

### Shadows of point and spot lights (ADR 0071)

Turn the feature on once, then mark the lights that cast shadows:

```rust
use engine_render::forward::{LocalLight, LocalShadowSettings};

renderer.set_local_shadows(&device, &mut allocator, &bindless,
                           Some(LocalShadowSettings::default()))?;
let lamp = LocalLight { shadow: true, ..lamp };
// ... and when the renderer is destroyed (the atlas lives in device memory):
unsafe { renderer.destroy(&device, &mut allocator, &bindless) };
```

- **Shadows are cached.** Each shadowed light gets tiles in one shadow atlas (a spot light
  one view, a point light six cube faces). They are drawn again only when the light moves
  or changes its range or cone, or when something within its range is added, moved,
  removed, gets another mesh or material, or a skinned mesh is posed differently. A still
  light in a still scene costs nothing but the lookups in shading.
- **Keep each light's index in the list stable.** The cache knows a light by its index:
  reordering only costs redraws, never wrong shadows (a light whose parameters differ is
  drawn again).
- **Moving lights share a budget.** At most `updates_per_frame` views (12) are redrawn per
  frame, oldest and largest first; the others keep last frame's shadow for a frame or
  more. A light seen for the first time is drawn at once. Raise the budget for many
  moving lights on a fast GPU; lower it on an integrated one (a view costs ~2.5 µs on an
  RTX 4060 Ti, ~70 µs on a 2-CU Radeon in Sponza).
- **Resolution follows the light's size on screen** (`texels_per_pixel`, between
  `min_resolution` and `max_resolution`), and all lights step down together when the
  atlas (`atlas_size`², 4096² D16 by default: 32 MiB) is full. Lights out of view keep
  their shadows until space is needed.
- **Prefer spot lights:** a point light costs six views to draw, and its shadow is looked
  up like a spot light's.
- **`renderer.local_shadow_stats()`** tells what the last frame did (lights in view,
  drawn, waiting for the budget, evicted); `renderer.invalidate_local_shadows()` forgets
  every shadow (after edits the renderer cannot see, such as a mesh's vertices replaced in
  place).
- **Views without shadowed lights pay nothing:** their shading has no code for it.

What it costs (ref-1, Sponza at 1080p, `scene_bench -- --sky --lights N --shadowed-lights N`):

| | RTX 4060 Ti | Radeon iGPU |
|---|---:|---:|
| forward, 16 lights without → with shadows (all cached) | 444 → 445 µs | 16.4 → 18.4 ms |
| redrawing all 16 lights (63 views) | 172 µs | 4.7 ms |
| 7 lights covering the whole screen, without → with shadows | 642 → 830 µs | 23.4 → 34.3 ms |

## 10. Sky light and the sky (image-based lighting)

An environment lights everything from the whole sky (ADR 0046):

- rough surfaces take its average color from their side;
- metals reflect it, blurred by their roughness;
- where nothing is drawn, the sky and the sun's disk appear.

```rust
use engine_render::environment::{Environment, Sky};

// Once: a sky of three colors (linear radiance) around the sun.
let environment = Environment::new(&device, &mut allocator, &bindless, Sky {
    zenith: Vec3::new(0.12, 0.25, 0.6),
    horizon: Vec3::new(0.55, 0.62, 0.72),
    ground: Vec3::new(0.12, 0.11, 0.1),
})?;

// Every frame:
let view = ForwardView { environment: Some(&environment), ..view };

// Evening: the next frame rebuilds the lighting.
environment.set_sky(evening);
environment.set_intensity(0.5);
```

- **Changing the sky costs one frame** a few compute passes: 0.05 ms on an RTX 4060 Ti,
  1.3 ms on a Radeon iGPU. Change it when time of day moves noticeably, not every frame.
- **With an environment, `DirectionalLight::ambient` is not used.** The sky replaces the
  flat ambient color.
- **The sun's disk is drawn in the sky** from the light's direction and color. Its light
  still comes from the directional light (with its shadows), not from the environment.
- **Occlusion textures** darken the sky light, diffuse and specular.
- **One environment per view per frame for now.** Give split-screen views their own.
- **Debugging:** `environment.set_debug_directions(true)` colors the sky by direction
  (x red, y green, z blue).

What it costs (ref-1, Sponza at 1080p): the forward pass goes from 0.32 to 0.40 ms on the
RTX 4060 Ti, and from 7.7 to 12.2 ms on the Radeon iGPU. That is the price of real
reflections on every surface. The sky pass itself costs 3 µs and 33 µs.

### A physical sky, sunsets, aerial perspective and fog

Instead of the gradient, an environment can carry a physically based atmosphere
(ADR 0069): the sky follows the sun, the sun reddens towards the horizon and sets, the sky
light matches the sky, and far objects fade into the air. Height fog works with either
sky.

```rust
use engine_render::atmosphere::{Atmosphere, HeightFog};

// Once (the first call makes the atmosphere's tables and pipelines).
environment.set_atmosphere(&device, &mut allocator, &bindless, Some(Atmosphere::earth()))?;
environment.set_fog(Some(HeightFog {
    density: 0.02,      // per world unit at base_height
    base_height: 0.0,
    falloff: 0.2,       // thins by e every 5 units up
    ..HeightFog::default()
}));

// Every frame: the directional light is the sun. Its color is the sunlight outside the
// atmosphere; the renderer dims and reddens it for the sun's height.
let light = DirectionalLight { direction: -to_sun, color: Vec3::splat(4.0), ambient: Vec3::ZERO };
let view = ForwardView { light, environment: Some(&environment), ..view };
```

- **Move the sun freely.** Its azimuth costs nothing; its height rebuilds the sky's table
  every 0.02° (0.01 ms on an RTX 4060 Ti, 0.3 ms on a Radeon iGPU) and the sky light
  every 0.1° (about 0.06 / 1.3 ms). Changing the atmosphere's parameters also rebuilds
  its other tables (0.03 / 0.8 ms more): fine for weather changes, not every frame.
- **Units.** The atmosphere is in kilometres; your scene is in metres by default
  (`meters_per_unit`). `origin_altitude` puts the world's origin above the ground (a
  mountain level). Air is thin: across a 50 m room nothing shows. For small scenes that
  should look vast, raise `aerial_perspective_scale` (distances count that many times).
- **`drives_sun: false`** keeps your light's color as it is (a stylized sun), while the sky
  still follows its direction.
- **Fog** is lit by the sky's average color and the sun (a glow towards it,
  `sun_anisotropy`); it casts no shadows (no light shafts yet). `start_distance` keeps it
  off the near field, `max_opacity` keeps distant shapes visible.
- **Transparent surfaces** get the aerial perspective and fog in their own shaders.
- **What it costs** (1080p): one full-screen pass after the opaque surfaces, 0.03 ms
  (0.06 with fog) on the RTX 4060 Ti, 1.1 ms (2.0 with fog) on the Radeon iGPU, replacing the
  sky pass; the forward pass does not change. Views without an atmosphere or fog pay
  nothing.

## 11. Bloom and automatic exposure

Before tone mapping, two passes make HDR frames read like photographs (ADR 0047):

- **automatic exposure** brings the scene's typical brightness to middle grey and adapts
  when it changes;
- **bloom** makes very bright light glow.

```rust
use engine_render::post::Composite;
use engine_render::postfx::{AutoExposure, Bloom, BloomSettings, ExposureSettings};

// Once per view:
let bloom = Bloom::new(&device, &bindless, BloomSettings::default())?;
let exposure = AutoExposure::new(&device, &mut allocator, &bindless, ExposureSettings::default())?;

// Every frame, after the scene (and TAA) into `hdr`:
let bloomed = bloom.add_passes(&mut fg, &bindless, hdr);
let measured = exposure.add_passes(&mut fg, &bindless, hdr, frame_seconds);
tonemap.add_composite_pass(&mut fg, &bindless, hdr, backbuffer, Composite {
    bloom: Some(bloomed),
    auto_exposure: Some(measured),
    exposure: 1.0, // now a compensation on top
    ..Composite::default()
});

// After a camera cut: no slow adaptation from the old place.
exposure.reset();
```

- **Exposure:**
  - `ExposureSettings::compensation` shifts it in EV (+1 twice as bright);
  - `speed_up` and `speed_down` set how fast it adapts to darkness and to light, in EV per
    second;
  - `low_percent` and `high_percent` choose which pixels count: by default the darkest
    10% and brightest 10% are ignored, so the sun or a black border does not drive it.
- **Bloom:**
  - `intensity` is the share of the image the glow replaces: 0.04 by default, no light is
    created;
  - `scatter` widens it;
  - `karis_average` (on) stops sparkling specular pixels from flickering in the glow, at
    the cost of small light sources glowing less than their energy.
- **The tone curve** (`Composite::curve`, ADR 0048):
  - Uchimura's by default: mid-tones kept, a filmic shoulder, the display's peak on HDR;
  - `ToneCurve::PbrNeutral` for material and product views;
  - `ToneCurve::Agx`: soft, low contrast; saturated highlights go to white along their
    hue (Blender's look);
  - `ToneCurve::Aces`: the classic contrasty film look of many games; mid-grey darker
    (add about +0.7 EV of exposure);
  - AgX and ACES are SDR curves: on HDR screens they do not use the extra range.
  - Compare them: `scene_bench -- <scene> --post --curve gt|pbr-neutral|agx|aces`.
- **What it costs** at 1080p: bloom 0.11 ms, exposure 0.02 ms, tone map 0.03 ms on an RTX
  4060 Ti. On a Radeon iGPU they are 1.1 ms, 0.28 ms and 0.99 ms: consider fewer bloom
  levels there.

## 12. Transparent surfaces

Materials with `AlphaMode::Blend` are drawn by a transparent pass after the opaque
surfaces and the sky (ADR 0054):

```rust
let glass = materials.add(MaterialDesc {
    base_color: [0.9, 0.95, 1.0, 0.25], // alpha 0.25: a quarter of the light is the glass's
    roughness: 0.05,
    alpha: AlphaMode::Blend,
    ..MaterialDesc::default()
});
draws.push(DrawItem { mesh: window_pane, material: glass, world_from_object });
```

- **Nothing else to do:** put them in the same draw list, in any order.
- **Alpha is coverage**, as glTF's `BLEND` says:
  - the whole shading, highlights included, scales with it;
  - alpha 1 looks like an opaque surface, still drawn in the transparent pass.
- **Order:** objects are sorted back to front by the distance to their box's center.
  - Separate objects blend correctly.
  - Objects that intersect, or one inside another, can blend in the wrong order in
    places: split them, or make the inner one opaque or masked.
  - Inside one mesh, triangles blend in index order. For a closed glass, draw the back
    faces and front faces as two meshes, or accept the error.
- **They are lit like the rest:** sun and its shadows, point and spot lights, sky light.
  - They cast no shadows.
  - They do not hide what is behind them from occlusion culling.
- **Under TAA/TAAU** the pass also writes a reactive mask (`MotionOutput::reactive`, R8,
  the layers' combined alpha). The resolve uses it by itself: history counts less where
  transparent surfaces cover, so they do not ghost over moving backgrounds.
  - The price is a little more shimmer on their edges.
  - Use the mask in your own temporal effects too.
- **What it costs:** each transparent object is its own draw command, about 37 ns of CPU
  per object per frame.
  - 10,000 transparent cubes: `add_passes` 0.45 ms instead of 0.08 ms.
  - GPU culling on a Radeon iGPU: 0.09 ms instead of 0.02 ms.
  - Hundreds are cheap. Thousands of small ones (smoke, sparks) belong to the particle
    system, which sorts on the GPU.
  - Measure with `draw_bench -- --blended`.
- **Not yet:** refraction and distortion, colored shadows through glass,
  order-independent transparency.

## 13. Ambient occlusion

Ambient occlusion darkens the sky light where the surroundings hide the sky: creases,
corners, the undersides of arches, folds of cloth (GTAO, ADR 0058). Turn it on per
renderer:

```rust
use engine_render::ao::{AoQuality, AoSettings};

renderer.set_ambient_occlusion(&device, &bindless, Some(AoSettings {
    quality: AoQuality::High, // 3 directions × 3 samples per side
    radius: 0.5,              // how far occluders count, in world units
    ..AoSettings::default()
}))?;
```

- **What it darkens:** the ambient light and the sky's diffuse and reflected light, never
  the sun or point and spot lights (their shadows are their occlusion).
  - Bright surfaces lose less than dark ones (light bounces inside the crease).
  - It multiplies with the material's own occlusion texture.
- **Radius is in world units:** 0.5 is half a metre in a metre-scaled scene. Larger radii
  darken wider areas at the same cost.
- **Quality and cost** at 1920 × 1080 on an RTX 4060 Ti: `Low` 0.23 ms, `Medium` 0.31,
  `High` 0.46 (default), `Ultra` 1.06. Under TAAU it runs at the render resolution, so
  upscaling makes it cheaper too.
  - `Low` and `Medium` are noisy per frame: use them with TAA/TAAU, which averages the noise
    (the noise changes per frame only when TAA is on, otherwise it would flicker).
  - `AoQuality::Custom { slices, steps }` for references and comparisons.
- **It turns the depth prepass on** (its depth must be ready before shading): about
  0.06 ms on Sponza.
- **Perspective cameras only:** orthographic views draw without it.
- **Look:** `power` (2.2) darkens or lightens the whole effect, `falloff` softens its outer
  edge, `denoise_passes` (0–3, 1) blurs it along surfaces without crossing depth edges.
- **Limits of screen space:** nothing off screen or hidden behind the nearest surface
  occludes, and thin objects (poles, railings) occlude everything behind them within the
  radius.
- Measure with `scene_bench -- --ao low|medium|high|ultra|<slices>x<steps>`.

## 14. Skin, wax and leaves: subsurface scattering

Light that enters skin, wax, marble, jade, fruit or leaves spreads under the surface and
comes out around where it entered (ADR 0059). Give the renderer the diffusion profiles,
and the materials a `Subsurface`:

```rust
use engine_render::material::{Subsurface, Transmission};
use engine_render::subsurface::SubsurfaceProfile;

// Profile 0: human skin at its real size (millimetres, a metre-scaled scene).
// Profile 1: wax, light spreading 8 mm in red, 5 in green, 3 in blue.
renderer.set_subsurface_profiles(&device, &bindless, &[
    SubsurfaceProfile::skin(),
    SubsurfaceProfile::from_radius([8.0, 5.0, 3.0]),
])?;
let face = materials.add(MaterialDesc {
    subsurface: Some(Subsurface {
        profile: 0,
        transmission: Transmission::Thick, // ears and nostrils light up from behind
        ..Subsurface::default()
    }),
    ..MaterialDesc::solid([0.85, 0.62, 0.52], 0.0, 0.45)
});
```

- **Profiles** belong to the renderer; materials name them by index. Call
  `set_subsurface_profiles` when they change (a level load), not every frame: the first
  call also creates the 9 pipelines subsurface materials need (about 0.1 s cold, a few
  ms from the pipeline cache). A game that never calls it never pays for them.
  - `skin()` is d'Eon and Luebke's measured six-Gaussian fit. `with_scale(k)` makes a
    profile `k` world units per millimetre: larger values look softer (stylized or
    miniature scenes); 0 turns scattering off.
  - `from_radius([r, g, b])`: one Gaussian per channel, 99% of the light within those
    millimetres. `SubsurfaceProfile::new` takes your own Gaussians.
- **`Subsurface` on a material** (opaque or masked; blended materials ignore it):
  - `mask`: how much of the diffuse light scatters (1: all).
  - `transmission`: `Thin` (leaves, paper: the material's `thickness`), `Thick` (solid
    objects: the thicker of `thickness` and the object itself as the sun's shadow map
    sees it), or `None`. Only the sun transmits.
  - `texturing`: `PostScatter` keeps the base color's detail sharp (pores, freckles);
    `PreAndPostScatter` blurs half of it with the light (scans whose texture already
    holds the scattering).
  - `thickness` and `thickness_texture` (green) in world units; glTF's
    `KHR_materials_volume` thickness is imported into `MaterialData`.
- **Settings:** `renderer.set_subsurface(Some(SubsurfaceSettings { quality }))`,
  `SubsurfaceQuality::Low | Medium | High | Ultra` = 8 / 20 / 40 / 80 samples per pixel
  (Medium by default); `None` shades subsurface materials as ordinary ones. Under TAA the
  noise changes every frame and averages out; without it the pattern is fixed.
- **What happens:** a view that draws subsurface materials runs the depth prepass, draws
  them in a "forward subsurface" pass after the others, then a "subsurface scatter" pass
  adds their spread light. Other materials are shaded exactly as before.
- **What it costs** at 1080p (`subsurface_bench`), scatter pass for skin a metre away /
  four times larger:
  - RTX 4060 Ti: 17–27 µs / 18–33 µs when it covers 10% of the screen (Low–Medium),
    108 / 137–256 µs at 100%;
  - Radeon iGPU: 0.4 / 0.5–1.0 ms at 10%, 3.9 / 5.3–9.6 ms at 100%: use `Low` there,
    and keep close-ups of skin moderate.
  - The forward subsurface pass costs 6–12% more than ordinary shading of the same
    pixels.
- **Limits:** only what is on screen scatters; two scattering objects at the same depth
  exchange a little light; the exit albedo is stored in 8 bits.

## 15. Async compute: compute passes beside graphics work

GPUs with a separate compute queue can run compute work while the graphics queue
rasterizes (ADR 0065). The render graph does this for passes you mark, and schedules them
itself: it moves the work they need earlier and lets the graphics work that does not lead
to them run alongside.

```rust
fg.add_compute("my blur", &bindless)
    .read(depth, Access::Sampled(ShaderStages::COMPUTE))
    .write(target, Access::StorageWrite(ShaderStages::COMPUTE))
    .async_compute() // may run on the async compute queue
    .dispatch(&blur, Dispatch::Threads([width, height, 1]), move |ctx| { /* push */ });
```

- **What runs there:** passes marked `.async_compute()` (on `add_compute` or `add_pass`)
  with no attachments, whose accesses are compute, transfer or indirect arguments. Others
  stay on the graphics queue; the result is the same either way.
- **Declare everything the pass touches.** The graph reorders passes only within the
  declared accesses; a pass that reads something it did not declare may run before or
  beside the pass that writes it.
- **On by default where it was measured faster:** NVIDIA GPUs. Off on AMD and other
  vendors: on the 2-CU Radeon iGPU the queues only take turns. Change it per graph with
  `graph.set_async_compute(true | false)`, or for the process with
  `ENGINE_ASYNC_COMPUTE=1` / `0`. Without a separate compute queue it is always off.
- **What the engine runs there:** ambient occlusion (beside the shadow maps: Sponza at
  1080p, 1.25 → 1.19 ms per frame on an RTX 4060 Ti) and the environment rebuild when the
  sky changes. Light tiles and automatic exposure stay on the graphics queue: too short
  to pay for the extra synchronization.
- **Mark a pass only after measuring it:** a pass that pairs with graphics work using
  different units (math- or memory-bound compute beside depth-only rendering) gains;
  short passes, or two cache-heavy passes, lose. Run your frame with
  `scene_bench --async-compute on` and `off` (or your own benchmark) on each GPU you
  target.
- **Reading the times:**
  - `graph.gpu_pass_times()` lists each pass with its `queue`; passes that overlap read
    longer, because they share the GPU;
  - `graph.gpu_frame_time()` is the frame from its first pass to its last: compare
    frames with this, not with the sum of the passes;
  - `graph.stats()` counts the async passes, the graph's own submissions and ownership
    transfers.
- **Submitting:** with async compute on, `fg.execute(...)` submits the frame's earlier
  parts to the device's graphics and compute queues itself. Submit the frame's command
  buffer to `device.graphics` afterwards as before; do not submit to these queues from
  other threads meanwhile.
- **Sharing between the queues:** resources move between queue families with ownership
  transfers by default; `graph.set_async_sharing(AsyncSharing::Concurrent)` (or
  `ENGINE_ASYNC_SHARING=concurrent`) creates the frame's transient resources for both
  families instead. Both measured the same on the RTX; exclusive keeps image compression.

## 16. Reflections: probes and screen-space reflections

Without them, every glossy surface reflects the sky (section 10): right outdoors, wrong
indoors, where a polished floor should show the room. Two features replace the sky's
reflections where they know better (ADR 0073):

- **Reflection probes:** boxes you place (one per room is typical). Each is a cube the
  engine renders from a point inside it, blurred per roughness, and kept until something
  in it changes. A surface inside a probe's box reflects that cube, corrected for the
  box's walls so reflections line up (parallax correction); probes blend over their edges,
  the smallest first; outside every probe the sky remains.
- **Screen-space reflections (SSR):** each glossy pixel traces its reflected ray through
  what is on screen this frame. Where the ray finds something it reflects it exactly
  (moving objects, small details no probe has); where it leaves the screen or goes behind
  something, the probes and the sky take over. Rough surfaces trace scattered rays that
  are filtered over space and time.

```rust
use engine_render::reflections::{ReflectionProbe, ReflectionProbes, ProbeSettings, SsrSettings};

// Once: the renderer's reflection passes (a renderer that never does this pays nothing).
frame.forward_mut().set_reflections(&device, &bindless, true)?;
// Once per scene: probes (captured by the next frames that draw the scene).
let mut probes = ReflectionProbes::new(&device, &mut allocator, &bindless, ProbeSettings::default())?;
let hall = probes.add(ReflectionProbe::room(Vec3::new(0.0, 2.0, 0.0), Vec3::new(12.0, 4.0, 6.0)));
// Per view: screen-space reflections are a setting (the view keeps their history).
settings.screen_space_reflections = Some(SsrSettings::default());
// Every frame: the probes come with the scene.
let scene = SceneInput { reflection_probes: Some(&probes), environment: Some(&environment), .. };
```

Without the frame renderer: `ForwardView { reflections: Reflections { probes: Some(&probes),
ssr: Some(&ssr) }, .. }`, with a `ScreenSpaceReflections` per view at the view's size.

- **They need an environment** (the sky is their last fallback) and a perspective camera.
  Views with reflections run the depth prepass. Transparent surfaces keep the sky's
  reflections.
- **Placing probes:** `ReflectionProbe::room(center, half_extents)` fits a box room. For
  other shapes set `influence` (where the probe is used, it fades in over
  `blend_distance` from its faces) and `parallax` (the box the reflections are projected
  on: usually the walls) apart; boxes can be rotated (`ProbeBox::rotation`). Capture from
  where the camera usually is (`position`).
- **When probes are captured again:** after `set`, `refresh(id)` / `refresh_all()`, when
  the sky or the sun changes, and, for scenes drawn from a `GpuScene`, when instances
  inside a probe's boxes move, appear or disappear. With draw lists, tell it yourself:
  `probes.invalidate_region(min, max)`. `ProbeRefresh::Manual` probes change only when
  asked. `faces_per_frame` (6 by default) spreads a capture over frames; the nearest
  waiting probe goes first.
- **Cost of a capture** (Sponza, 1080p view, 128² probes): a whole probe 0.23 ms on an
  RTX 4060 Ti, 3.1 ms on a Radeon iGPU (one face per frame: 0.08 / 1.0 ms). A static scene
  captures nothing after the first frames.
- **Cost every frame** (Sponza at 1080p): probes cost one full-screen pass (RTX 4060 Ti
  0.14 ms, Radeon iGPU 4.5 ms with 6 probes). SSR at half resolution costs about 0.3 ms on
  the RTX and 7.5 ms on the Radeon iGPU (full resolution: 0.65 / 18 ms). On small
  integrated GPUs prefer probes alone, or render at a lower size with an upscaler.
- **`SsrSettings`:** `half_resolution` (default on: a quarter of the rays), `max_roughness`
  (0.6: rougher surfaces use probes; reflections fade towards it), `steps` (64), `thickness`
  (how thick objects are assumed behind what the depth buffer shows), `temporal` (the
  current frame's share for glossy surfaces). After a camera cut call `reset` (the frame
  renderer does it on `ViewCamera::cut`).
- **Debugging:** `probes.stats()` (captured, waiting, faces drawn last frame, visible);
  `ForwardPasses::reflections` gives the normals, weights and SSR result to look at.
