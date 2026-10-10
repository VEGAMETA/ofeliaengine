# GPU particles

Research for queue item 1 (GPU particles and GPU radix sort), 2026-10-11. The engine had
no particles; the dev guide promised them. Decision:
[ADR 0081](../adr/0081-gpu-particles.md). The sort they need:
[gpu-sort.md](gpu-sort.md).

## Sources (checked 2026-10-11)

Designs only: no code was carried over.

| Source | License | How | What we take |
|---|---|---|---|
| Wicked Engine, `EmittedParticleSystem` ([turanszkij/WickedEngine](https://github.com/turanszkij/WickedEngine), `emittedparticle_*CS.hlsl`) | MIT | A particle buffer, a dead list of free slots, two alive lists that take turns, a counter buffer and indirect arguments; kickoff → emit → simulate (forces, colliders, depth-buffer collisions, frustum culling, squared distance for sorting) → sort → indirect draw; atomics to append | The lists, the turns, the GPU-only counts, indirect dispatch and draw |
| G. Thomas (AMD), "Compute-based GPU Particle Systems", GDC 2014 | talk | Dead/alive lists, emission and simulation in compute, a bitonic sort, tiled rendering of particles to cut overdraw | The lists; the tiled rendering kept for later (overdraw is the cost of large particles) |
| Godot 4, `GPUParticles3D` ([godotengine/godot](https://github.com/godotengine/godot), `particles_storage.cpp`, `particles.glsl`) | MIT | A process shader per material (user code) run per particle in compute; sorting by distance, lifetime or none; trails | Sorting by view depth; user process code later (with the material graph) |
| Unreal Engine 5, Niagara GPU simulation (public docs) | product docs | GPU emitters with fixed bounds, GPU sorting of translucent particles, depth-buffer collisions | Fixed capacities, GPU sorting |

## What we chose and why

- **The state lives on the GPU.** The CPU never reads a count back: a frame's spawns are
  clamped to the free slots on the GPU, and every dispatch and the draw size themselves
  indirectly. A readback would stall a frame or lag one behind.
- **Deterministic particles.** A particle's random numbers come from its birth number and
  its emitter's seed, not from the slot an atomic gave it. Equal depths sort by birth
  number (the alive list's order comes from atomics). The same emitters and steps give
  byte-identical images; the golden shot checks it on both GPUs.
- **Unlit, premultiplied, soft.**
  - The draw is one pass of camera-facing quads over the scene, blended premultiplied, so
    one pipeline does blended and added particles.
  - Particles test the scene's depth themselves: a texel read also fades them where they
    near the scene (soft particles), with no depth attachment.
  - They write the reactive mask when the view has one.
- **Not now:**
  - lighting (shadows, local lights and the environment on smoke);
  - collisions with the depth buffer;
  - user simulation code;
  - trails, meshes as particles;
  - low-resolution drawing of large particles (overdraw on the iGPU);
  - ECS components;
  - motion vectors (particles ghost under TAA where the view has no reactive mask).

## Measured

See ADR 0081.
