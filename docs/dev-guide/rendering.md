# 3D rendering: drawing many objects

The forward renderer draws on the GPU's initiative (ADR 0034). You hand it a list of
objects each frame. The GPU then:

- throws away what the camera cannot see;
- draws everything that shares a mesh with one instanced draw call.

The CPU cost of a frame barely depends on how many objects the list holds.

## 1. The draw list

```rust
use engine_render::forward::{DrawItem, Material};

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
    &mut fg, &mut ring, &bindless, &draws, &camera, &light, hdr, depth, Some(clear),
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

## 3. What it costs

`cargo run -p engine_render --release --example draw_bench` prints the costs for 1k, 10k
and 100k cubes, with the camera above the field and inside it, culling on and off:

- CPU time of building your list;
- CPU time of `add_passes` and of recording the frame;
- GPU time of the culling, shadow and forward passes;
- how many cubes were visible.

The GPU workflow runs it on the reference machine, and the results are in
`docs/perf/history.md`.

Rules of thumb:

- the CPU pays about 80 bytes of upload and a few dozen nanoseconds per object;
- the GPU culls millions of objects per millisecond;
- the forward pass then costs what is on screen;
- the shadow pass costs every object in the scene: shadows are fitted to the whole
  scene until cascaded shadow maps arrive.
