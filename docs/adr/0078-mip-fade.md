# 0078. Mip fade: streamed texture levels sharpen in over a short fade

- Status: Accepted
- Date: 2026-10-10
- Related: [ADR 0057](0057-texture-streaming.md) (levels streamed into the texture pool),
  [ADR 0076](0076-gpu-memory-budget-and-streaming.md) (the budget and feedback),
  `docs/research/streaming-fast-turns.md` (queue item 0b)

## Context

When a finer level of a streamed texture arrives, the texture pool gives the texture a
view over the new resident levels and the picture sharpens in one frame: a visible pop.
The fast-turn measurement (`stream_bench --turn`, research note above) found that on first
sight a few percent of the screen waits 0.08–0.15 s for its levels, and the owner asked for
mip fade (2026-10-10): show the arriving level over a short fade instead of at once.

Engines have done this for long: Unreal Engine 3 faded mips in and out
(`MipFadeInSpeed0` / `MipFadeOutSpeed0` in its ini files); D3D11 has
`SetResourceMinLOD` (2009) and D3D12 `ResourceMinLODClamp` for clamping a resource's
finest level, which `VK_EXT_image_view_min_lod` brings to Vulkan. No patent was found on
fading a texture's minimum level (search 2026-10-10). Both reference GPUs expose the
extension with its `minLod` feature (RTX 4060 Ti 616.92, Radeon 26.8.1), and
`shaderResourceMinLod`.

## Options

1. **The view's minimum level** (`VkImageViewMinLodCreateInfoEXT`, chosen). The pool
   already makes a new view and bindless index each time the resident levels change; a
   fade makes a few more, each starting a fraction of a level finer, and the sampler's
   trilinear filter blends the two levels between. No shader changes: no register costs
   anywhere (CLAUDE.md §5, the AMD forward shader sits at the 64-VGPR line), nothing for
   textures that do not fade.
2. **A clamp in the shader** (`Sample(..., clamp)`, the `MinLod` operand under
   `shaderResourceMinLod`, from a per-texture value in a buffer): smooth every frame, no
   new views; but every material sample gains an operand and a load, in every shader and
   view, whether anything fades or not.
3. **Two samples during a fade** (old and new views, blended by a per-texture factor):
   twice the texture cost while fading, and the same register cost everywhere.
4. **The sampler's minimum level:** per sampler, not per texture.

## Measurements

ref-1, 2026-10-10:

- **Stepping** (`engine_render` test `a_new_level_fades_in_a_quarter_level_at_a_time`,
  four frames a level): the arriving frame still shows level 3; then 2.75, 2.5, 2.25, 2.0,
  a new view and bindless index at each step and none after; with fades off the next
  level shows at once. Both GPUs, under synchronization validation.
- **On screen** (`engine_frame` test `a_streamed_level_fades_in_on_screen`: level 0 red,
  level 1 green, the face sampled at about level 0; red's share of red and green):
  RTX 0.000 → 0.229 → 0.500 → 0.771 → 1.000 (quarters, as asked); Radeon 0.000 → 0.000 →
  0.500 → 1.000 → 1.000: its driver applies the view's fractional minimum in halves, though
  both report 8 `mipmapPrecisionBits`. A fade on the Radeon is two steps a level.
- **Cost** (`stream_bench`, the Sponza flight, 900 frames at 60 fps, NVMe pack): 756–760
  views made by fade steps over the flight (under one a frame); with and without fades,
  RTX GPU frame median 4.26–4.29 / 4.38–4.49 ms and CPU frame 5.40–5.51 / 5.61–5.69 ms
  (paced: power states), Radeon 12.345 / 12.343 ms and 13.34 / 13.32 ms; streaming work
  on the frame's thread 0.019–0.024 ms either way. No GPU cost: the views read the same
  levels with the same filter.
- **Settled frames allocate nothing** (`engine_streaming` `zero_alloc`, both GPUs) once
  fades are counted as streaming work; a fade step allocates only when it retires a view.

## Decision

- `TexturePool::set_mip_fade(Option<MipFade>)`: `MipFade::Time(per level)` for games,
  `MipFade::Frames(n)` for deterministic tests. Default `MipFade::DEFAULT`: 100 ms a level
  (one to three levels arrive together: 0.1–0.3 s, about as long as they took to arrive).
- A fade moves in quarters of a level: each step a new view (`fade_step`) and bindless
  index; the old view is retired after its frames, as before. `TexturePool::fading`,
  `shown_level` and `fade_views` report it.
- Only levels that join a picture already shown fade: a texture's first levels show at
  once (before them the material draws its factor alone), and evicted levels go at once
  (eviction mostly hits textures not seen recently). A reallocation (`set_base`) keeps a
  fade going: the new image's view starts where the fade was.
- Without `VK_EXT_image_view_min_lod` (`Device::image_view_min_lod`) levels show at once.
- Captures turn fades off (`tests/golden`'s test bed); the streaming tests fade by frames
  and count fading textures as unsettled.

## Consequences

- A texture is fully sharp up to 100 ms a level later than its level arrived: the fade
  trades that for no visible pop.
- On the Radeon the fade has two visible steps a level instead of four; half its views
  change nothing on screen there (cheap: under one view a frame in the flight).
- The first appearance of a texture (from the material's flat factor to its tail) still
  pops: fading it needs the shader (option 2 or 3), left out.
- More views and bindless indices pass through the retire lists while fading; the
  bindless set's capacity is unaffected in steady state.
