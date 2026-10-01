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
- **Known limits for now:**
  - no texture LOD bias while upsampling (textures are slightly softer than native);
  - transparent surfaces lower the history's weight where they cover (section 12), but
    there is no mask for animated textures yet;
  - no sharpening.

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

## 9. Point and spot lights

Any number of local lights (ADR 0045), up to 65,535 per view:

```rust
use engine_render::forward::{LocalLight, Spot};

let lights = [
    // A lamp: color × intensity, and the distance where its light ends.
    LocalLight { position: Vec3::new(2.0, 3.0, -4.0), range: 8.0,
                 color: Vec3::new(1.0, 0.8, 0.6) * 20.0, spot: None },
    // A spot pointing down, full between 0.3 rad of its axis, fading out by 0.5.
    LocalLight { position: Vec3::new(0.0, 5.0, 0.0), range: 12.0, color: Vec3::splat(40.0),
                 spot: Some(Spot { direction: Vec3::NEG_Y, inner: 0.3, outer: 0.5 }) },
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
- **Not yet:** shadows of local lights, and physical light units.

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
