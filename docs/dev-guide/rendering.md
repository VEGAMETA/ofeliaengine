# 3D rendering: drawing many objects

The forward renderer draws on the GPU's initiative (ADR 0034). You hand it a list of
objects each frame. The GPU then:

- throws away what the camera cannot see;
- draws everything that shares a mesh with one instanced draw call.

The CPU cost of a frame barely depends on how many objects the list holds.

## 1. The draw list

```rust
use engine_render::forward::{DrawItem, ForwardView, Material};

draws.clear();
for (transform, look) in scene_objects {
    draws.push(DrawItem {
        mesh: &rock_mesh,
        world_from_object: transform,
        material: Material {
            base_color: look.color,
            metallic: 0.0,
            roughness: 0.8,
        },
    });
}
let passes = renderer.add_passes(
    &mut fg,
    &mut ring,
    &bindless,
    &draws,
    &ForwardView {
        camera,
        light,
        color: hdr,
        depth,
        clear: Some(clear),
        occlusion: None, // or Some(&occlusion): see section 3
    },
);
```

- **Keep objects with the same mesh next to each other in the list.**
  - Every run of consecutive items with the same `&Mesh` is one batch, and one batch is
    one instanced draw.
  - 10,000 rocks followed by 500 trees make 2 batches. The same objects interleaved make
    up to 10,500.
  - Both render correctly, but the first is the fast one.
- **Reuse the `Vec`** (`clear()` and refill) so a frame makes no heap allocations.
- **Transforms are affine.** Translation, rotation and scale, including mirroring. The
  bottom row of the matrix is ignored.
- **The upload ring must fit the frame.** Size it with about 100 bytes per object plus a
  little per mesh.
  - `add_passes` returns `None` when the ring is full. It then declares nothing, and the
    frame has no 3D content.

## 2. Culling

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
    command per batch;
  - after the frame, `instance_count` in each command is the number of visible objects.
  - `crates/engine_render/tests/culling.rs` and `examples/draw_bench.rs` show how to read
    it back.

## 3. Occlusion culling: skipping what is hidden

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
    culling pass, 25-35 µs on an RTX 4060 Ti, 215-250 µs on a Radeon iGPU at 1080p). On the
    iGPU the city got 9% faster; on the RTX, with cubes that cost almost nothing to draw,
    it got slower. Detailed meshes and expensive materials change that.
  - In an open field nothing hides anything, and the extra passes are pure cost.
  - Turn it on per camera, and compare with `draw_bench` on the GPUs you target.
- The capacity is the number of draw items whose visibility is remembered. Items beyond
  it still render correctly, but are always tested in the late phase.
- Free the state with `unsafe { occlusion.destroy(&mut allocator) }` once the GPU is idle.

## 4. What it costs

`cargo run -p engine_render --release --example draw_bench` prints the costs for 1k, 10k
and 100k cubes, with the camera above the field, inside it and in a city of walls, and
culling off, frustum only, or frustum and occlusion:

- CPU time of building your list;
- CPU time of `add_passes` and of recording the frame;
- GPU time of the culling, depth pyramid, shadow and forward passes;
- how many objects were drawn.

Results from the reference machine are in `docs/perf/history.md` and ADR 0035.

Rules of thumb:

- the CPU pays about 80 bytes of upload and a few dozen nanoseconds per object;
- the GPU culls millions of objects per millisecond;
- the forward pass then costs what is on screen;
- the shadow pass costs every object in the scene: shadows are fitted to the whole
  scene until cascaded shadow maps arrive.
