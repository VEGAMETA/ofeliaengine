# 0081. GPU particles: a pool on the GPU with dead and alive lists, sorted by depth, drawn soft

- Status: Accepted
- Date: 2026-10-11
- Research: [docs/research/gpu-particles.md](../research/gpu-particles.md)
- Related: [ADR 0080](0080-gpu-radix-sort.md) (the sort), [ADR 0052](0052-compute-passes.md),
  [ADR 0072](0072-frame-renderer.md) (the hook the draw goes in), queue item 1

## Context

The engine drew no particles: no sparks, smoke, dust or magic. The dev guide promised
GPU particles, and the queue put them first after the core work.

## Options

1. **CPU simulation, GPU draw:** easy to script, but a CPU cost per particle every frame,
   and an upload per frame.
2. **GPU simulation whose counts the CPU reads back:** simpler dispatches, but a stall or
   a frame of lag.
3. **GPU simulation with GPU-only counts** (chosen; Wicked Engine's and AMD's design): a
   dead list of free slots and two alive lists taking turns, counters and indirect
   arguments on the GPU. The CPU only says what each emitter spawns.

For drawing transparent particles in the right order: sort by view depth on the GPU
(ADR 0080's radix sort), or draw unsorted with additive or weighted blending only. The
sort is chosen: smoke must be blended over what is behind it.

## Measurements

`particle_bench` (ref-1, release, 1920 × 1080): µs per pass, median of 30 frames after
the pool filled.

| alive | kind | RTX 4060 Ti: simulate, sort, draw | Radeon iGPU: simulate, sort, draw |
|---:|---|---:|---:|
| 16 K | sparks (small, added) | 15, 93, 20 | 91, 328, 227 |
| 64 K | sparks | 43, 110, 68 | 512, 1,072, 843 |
| 256 K | sparks | 172, 210, 256 | 2,176, 3,883, 3,369 |
| 1 M | sparks | 910, 525, 942 | 17,372, 11,012, 18,556 |
| 256 | smoke (large, blended) | 10, 82, 98 | –, 99, 1,575 |
| 1 K | smoke | 10, 83, 399 | –, 117, 6,548 |
| 4 K | smoke | 10, 84, 1,575 | –, 168, 25,733 |
| 16 K | smoke | 13, 85, 6,186 | 83, 331, 101,725 |

- Small simulation passes run beside the bench's clear and leave no time of their own
  ("–").
- The sort dominates small counts: 32-bit keys, 41 dispatches. With 16 bits of depth and
  16 of birth number, equal depths keep their birth order.
- Large particles cost their overdraw. 256 smoke puffs over 1080p take 1.6 ms on the
  Radeon. Drawing them at a lower resolution is the known cure (not built).
- **Correctness test**, on both GPUs with synchronization validation clean:
  - alive + dead is the capacity after every step;
  - the alive count is exactly the rate × the lifetime (steps of 1/64 s, lifetimes of
    0.5 s: exact in binary);
  - each particle is where semi-implicit Euler puts it;
  - two runs give the same particles.
- **Golden shot** `procedural_particles` (sparks and smoke over the procedural scene,
  90 steps): byte-identical from run to run on each GPU.

## Decision

- **`engine_render::particles::ParticleSystem`**, a pool of a fixed capacity: 48 bytes
  per particle and 12 bytes of lists, plus a frame's sort.
  - Emitters: `add_emitter(ParticleEmitter)`, `emitter_mut`, `burst`, `remove_emitter`
    (its particles die), `reset`.
  - An emitter spawns `rate` per second (fractions carried) and bursts, in a ball,
    into a cone. Its particles have an acceleration, a drag, a spin, size and color from
    birth to death, and a texture or a soft disc. `additive` is the share added instead
    of blended. They fade over `soft_distance` before the scene.
  - Per frame: `prepare(ring, dt, eye, forward, far)` uploads the emitters (before the
    view); `add_passes(fg, bindless, frame, target)` declares the simulation (emission
    included), the sort and the draw. The draw goes in `FrameRenderer::add_view_with`'s
    hook, with the jittered camera and the view's reactive mask.
- **Shaders** (`particles.slang`):
  - `particlesBegin` → `particlesEmit` → `particlesSimulateArgs` → `particlesSimulate`
    (wave-aggregated appends) → `particlesFinish`;
  - `particlesVertex` / `particlesFragment` (`…Reactive` also writes the mask).
- **Determinism:** random numbers from the birth number and the emitter's seed; sort
  keys of 16 bits of depth (farthest first) and 16 of birth number.

## Consequences

- Effects are a few calls in a game. Their CPU cost is the emitter table: 176 bytes per
  emitter per frame.
- On the integrated GPU, about 64 K sparks or a few hundred large smoke puffs fit a
  frame's budget; more needs the measures below.
- Not done:
  - lighting of particles (shadows, local lights, the environment);
  - depth-buffer collisions;
  - user simulation code;
  - trails and mesh particles;
  - drawing large particles at a lower resolution;
  - a one-workgroup sort for small counts;
  - ECS components and extraction;
  - motion vectors (the reactive mask: see the amendment);
  - orthographic views (not drawn);
  - an emitter's search over hundreds of emitters (linear per spawn).

## Amendment (2026-10-11): the reactive mask for effects

`ForwardRenderer::set_reactive_effects(true)` gives every view with motion vectors a
cleared, single-sampled reactive mask even when no transparent surface drew one. The
particles mark it, so temporal antialiasing and the upscalers do not smear them. Before,
a view without transparent surfaces of its own had no mask, and under TAA its sparks came
out dimmer and smeared. The golden shot `procedural_particles_taa` (TAA, 90 frames) uses
it, byte-identical from run to run on both GPUs. It costs a clear of an 8-bit mask at the
render size, in views of a renderer that turned it on.
