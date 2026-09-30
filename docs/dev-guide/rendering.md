# 3D rendering: meshes, materials and many objects

The forward renderer draws on the GPU's initiative (ADR 0034). You hand it a list of
objects each frame, each a mesh, a material and a transform. The GPU then:

- throws away what the camera cannot see;
- draws everything that shares a mesh and a kind of material with one instanced draw
  call.

The CPU cost of a frame barely depends on how many objects the list holds.

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
- **Alpha:**
  - `AlphaMode::Mask { cutoff }` discards texels below the cutoff, in the view and in
    shadows (leaves, fences).
  - Blended transparency comes with the transparency pass. Until then import glTF's
    `BLEND` as opaque.
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
    command per batch (ordered by kind of material, then front to back);
  - after the frame, `instance_count` in each command is the number of visible objects.
  - `crates/engine_render/tests/culling.rs` and `examples/draw_bench.rs` show how to read
    it back.

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
- the shadow pass costs every object in the scene: shadows are fitted to the whole
  scene until cascaded shadow maps arrive.

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
- **Known limits for now:**
  - no texture LOD bias while upsampling (textures are slightly softer than native);
  - no masks for transparent or animated-texture surfaces;
  - no sharpening.

What it costs (ref-1, 1080p):

- **Motion vectors:** +8-23% on the forward pass (Sponza, FlightHelmet). Measure with
  `scene_bench -- --motion`.
- **The resolve:**
  - RTX 4060 Ti: 0.17 ms for TAA, 0.13 ms from 720p;
  - Radeon iGPU: 4.5 ms either way.
  - Measure with `cargo run -p engine_upscale --release --example taau_bench`.
- **Upsampling from 720p** is cheaper than native 1080p with TAA on both GPUs:
  - RTX: 0.19 ms against 0.30 ms for forward + resolve in that benchmark;
  - Radeon: 5.8 ms against 7.7 ms.
