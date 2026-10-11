# 0082. Decals: a decal buffer before lighting

- Status: Accepted (option 2, the owner, 2026-10-11)
- Date: 2026-10-11
- Research: [docs/research/decals.md](../research/decals.md)
- Related: [ADR 0045](0045-local-lights.md) (z-bins and tile bitmasks),
  [ADR 0058](0058-ambient-occlusion.md) (the register rule), [ADR 0079](0079-pipelines-on-first-use.md)
  (variants made on first use), queue items 2 and 7c (forward.rs split by feature)

## Context

Queue item 2 asks for clustered decals. The engine has none. Decals change a surface's
material before it is lit (albedo, roughness, metalness, normal). Each feature of the
forward shading is a literal parameter of `shade_surface` with entry points of its own,
so a view without the feature pays nothing, not even registers (CLAUDE.md §5).

## Options

1. **Clustered decals in the shading** (DOOM 2016, Godot 4, Bevy 0.16):
   - decals are binned with the lights' z-bins and tile bitmasks, and each shading
     invocation walks its pixel's decals before lighting;
   - every surface takes them, transparent ones included, with or without the prepass;
   - **cost:** a `decals` literal doubles the 32 fragment entry points and the forward
     renderer's pipeline sets (plain and multisampled).
     - Made on first use since ADR 0079, so start-up does not grow; shader build time
       and forward.rs's size do.
     - forward.rs (5,600 lines) would best be split by feature (queue item 7c) first.
2. **A decal buffer before lighting** (deferred decals, as in Killzone 2, Frostbite and
   Unreal's DBuffer):
   - after the depth prepass, a compute pass evaluates each pixel's decals (the same
     bins) into one or two render-size textures: albedo with coverage, roughness and
     metalness, a normal;
   - the shading reads them at its pixel: a variant still, but a texel load and a blend
     instead of a loop;
   - **cost:** the prepass whenever decals draw (it is on with AO and in the iGPU
     defaults), about 8 to 12 bytes per pixel, and transparent surfaces get no decals.
3. **Mesh decals** (a box or a projected mesh drawn after the opaque surfaces, blended
   into the lit color): no shading change, but the decal's albedo is not lit as the
   surface's, and normal and roughness cannot change. Rejected.

## Recommendation (to confirm)

Option 1, after the forward.rs split (7c), because it is what the clustered renderers it
is modelled on do, it decals everything, and ADR 0079 removed its start-up cost. Option 2
if the owner prefers a smaller change to the shading now. Measurements (decal cost per
pixel on both GPUs, registers of the decal variants) come with the prototype.

## Decision

**Option 2, a decal buffer before lighting** (the owner, 2026-10-11: "a decal buffer is
necessary; without it the corners would come out wrong"). After the depth prepass, a
compute pass evaluates each pixel's decals, binned like the lights (z-bins and tile
bitmasks), from the pixel's position and normal in the depth buffer, into render-size
targets (albedo with coverage, a normal, roughness and metalness); the forward shading's
decal entry points read them at their pixel and blend before lighting.

## Consequences

- Decals draw only with the depth prepass (on with AO and in the iGPU defaults); a view
  with decals turns it on.
- 8 to 12 bytes per pixel of transient targets while decals draw; nothing without them
  (the register rule: entry points of their own, made on first use, ADR 0079).
- Transparent surfaces get no decals.
- Measurements with the prototype: the decal pass and the shading's extra cost per pixel
  on both GPUs, the registers of the decal variants.
