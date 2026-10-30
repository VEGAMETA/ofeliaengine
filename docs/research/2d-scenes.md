# 2D scenes: tile maps, auto-tiling, atlases, 2D lights, tile collision, pixel-perfect cameras

Date: 2026-10-30. Input for [ADR 0125](../adr/0125-2d-scenes-and-tools.md) (E13, the 2D
row of [ADR 0107](../adr/0107-editor.md) §7); the editor's side is in
[the 2D tools' UX note](ux/2d.md).

The code of Godot, Bevy, bevy_ecs_tilemap, bevy_light_2d and LDtk (MIT; Bevy MIT OR
Apache-2.0) and of the vendored Box2D (MIT) was read for ideas only: **no code was copied**.
Tiled's editor is GPL: it was studied from its manual only, **no Tiled code was read or
carried over**. Unity, Unreal, Phaser and Aseprite were studied from their documentation.
Nothing was built or run for this note: numbers are measurements already recorded (ADR
0015, ADR 0016, `docs/perf/history.md`) or estimates marked as such, each with the step that
measures it.

## Where we start

- `engine_render2d` (ADR 0015): one 64-byte quad for sprites and SDF shapes, its bindless
  texture inside it, batches split only by blend pipeline and clip rectangle; order by
  layer, key and submission, radix-sorted with no allocation. ref-1, 1080p, 8-pixel sprites
  (64 pixels each): the RTX draws 1,000,000 in 1.64 ms (1.6 ns a sprite, about 0.026 ns a
  covered pixel), the Radeon in 23 ms (indexed is slower there; a plain list 23 ns a sprite,
  about 0.36 ns a covered pixel; 10,000 in 0.365 ms, 0.57 ns a pixel). Recording costs 7 ns
  a quad, a y-sort 5.3–6.3 ns, copying into the ring 1.0–2.3 ns (`benches/canvas.rs`);
  preparing and uploading 100,000 quads 1.14 ms into the RTX's BAR memory, 0.76 ms into the
  Radeon's. These per-pixel figures were measured into an 8-bit target.
- The frame renderer's scene colour is `R16G16B16A16_SFLOAT`: a blended full-screen layer
  reads and writes 16 bytes a pixel, 33 MB at 1080p, twice the bytes of ADR 0015's target.
- Viewports (ADR 0016) scale a pixel-art image by whole numbers (2.0 µs on the RTX, 5.9 µs
  on the Radeon for the example's); the frame renderer renders at a fixed `render_size`
  and scales by integers, nearest, centred (`Upscale::Integer`).
- The scene's `Camera` is perspective only. Culling and the light tiles already accept an
  orthographic `clip_from_world` (`cull.rs`, `lights.rs`), but particles skip orthographic
  views (ADR 0081) and the cascades and AO were built and tested for perspective views
  only. The player draws `engine_render2d` canvases for the game's UI only (`Game::ui`),
  after the post chain.
- Box2D v3.1.1 is vendored (commit `8c661469`, MIT); the shim gives boxes, circles and
  capsules, casts, overlaps and begin-contact events. Nothing outside `engine_physics2d`
  uses it.
- The bindless binding is declared `Texture2D`, `Texture2DArray` and `TextureCube` by the
  shaders that need each (descriptor aliasing); the texture pool makes one-layer images
  only.
- The tone curves (`engine_render::post::ToneCurve`: Uchimura's, PBR Neutral, AgX, ACES)
  all change colours on purpose; none leaves a colour as it was painted.

## Sources

| Source | Version / date checked | Licence | What we looked at |
|---|---|---|---|
| [Godot](https://github.com/godotengine/godot) `modules/tilemap/tile_map_layer.{h,cpp}`, `tile_set.cpp`, `editor/tile_map_layer_editor.cpp`, `editor/tiles_editor_plugin.cpp`; `servers/rendering/renderer_rd/renderer_canvas_render_rd.{h,cpp}`, `shaders/canvas_sdf.glsl`; `editor/scene/2d/sprite_2d_editor_plugin.cpp` | master `7be59b46` (2026-10-30), read 2026-10-30 | MIT | rendering quadrants, the cell data format, physics quadrants and polygon merging, the terrain solver, the 2D shadow atlas, lights per item, the 2D SDF |
| Godot 4.7 docs: [TileMapLayer](https://docs.godotengine.org/en/stable/classes/class_tilemaplayer.html), [Using TileSets](https://docs.godotengine.org/en/stable/tutorials/2d/using_tilesets.html), [Using TileMaps](https://docs.godotengine.org/en/stable/tutorials/2d/using_tilemaps.html), [Multiple resolutions](https://docs.godotengine.org/en/stable/tutorials/rendering/multiple_resolutions.html), [2D sprite animation](https://docs.godotengine.org/en/stable/tutorials/2d/2d_sprite_animation.html) | checked 2026-10-30 | CC BY 3.0 (docs) | texture padding, terrain modes, painting tools, integer scaling, the two flipbook paths |
| Godot forum: [Large TileMapLayer nodes cause issues with merging git branches](https://forum.godotengine.org/t/large-tilemaplayer-nodes-cause-issues-with-merging-git-branches/136301) (4.5.1); [get_coords_for_body_rid in 4.5](https://forum.godotengine.org/t/s-this-a-bug-in-4-5-or-my-code/123489) | checked 2026-10-30 | — | a layer's cells as one 590,000-character line; physics quadrants seen by users |
| [Bevy](https://github.com/bevyengine/bevy) `crates/bevy_sprite_render/src/tilemap_chunk/{mod.rs,tilemap_chunk_material.wesl}`; `crates/bevy_image` (atlas builders) | main `ae615215` (2026-10-29), v0.20.1, read 2026-10-30 | MIT OR Apache-2.0 | a chunk as one quad with a tile-data texture and a tile-set array texture; atlas packing |
| [bevy_ecs_tilemap](https://github.com/StarArawn/bevy_ecs_tilemap) `src/map.rs`, `src/render/chunk.rs`, `src/render/shaders/tilemap_vertex.wesl` | main `9d9ec118`, 0.20.0 (2026-10-21), read 2026-10-30 | MIT | an entity per tile, 64 × 64 meshed chunks, animation in the vertex shader, grid shapes |
| [bevy_light_2d](https://github.com/jgayfer/bevy_light_2d) `src/render/sdf/sdf.wesl`, `src/render/light_map/light_map.wesl` | main `dd9643c3` (2026-10-09), 0.11, read 2026-10-30 | MIT | occluders as a full-screen distance field, a ray march per light |
| [Phaser 4.1 `TilemapGPULayer`](https://docs.phaser.io/api-documentation/class/tilemaps-tilemapgpulayer) (documentation) | 4.1.0, checked 2026-10-30 | MIT | a whole layer as one quad, its limits |
| B. Jones, "[Sprite Tile Maps on the GPU](https://blog.tojicode.com/2012/07/sprite-tile-maps-on-gpu.html)" (2012-07-30); [gl-tiled](https://englercj.github.io/2018/01/07/gl-tiled) (2018), built on it | read 2026-10-30 | the demo's licence is not stated: ideas only | the map as a texture of tile coordinates, one quad, a lookup |
| [LDtk](https://github.com/deepnight/ldtk) `src/electron.renderer/data/def/AutoLayerRuleDef.hx`, `data/inst/LayerInstance.hx`, `ui/RulePatternEditor.hx`, `ui/modal/dialog/RulesWizard.hx`; [JSON schema](https://ldtk.io/json/) | master `2b7b5512` (2026-10-06), v1.5.3 (JSON 1.5.3), read 2026-10-30 | MIT | auto-layer rules: patterns, chance, flips, break on match, what an edit re-runs, what the file keeps |
| [Tiled manual](https://doc.mapeditor.org/en/stable/manual/terrain/): Terrain, Editing Tile Layers | Tiled 1.12.2 (2026-05-27), checked 2026-10-30 | the editor is GPL: **manual only, no code read** | corner, edge and mixed sets; the terrain brush; probabilities; turned tiles |
| [Better Terrain](https://github.com/Portponky/better-terrain) README | checked 2026-10-30 | Unlicense | why a Godot plugin replaces the built-in terrains |
| Boris the Brave, "[Classification of Tilesets](https://www.boristhebrave.com/2021/11/14/classification-of-tilesets/)" (2021-11-14, updated 2025-02-28), "[Tileset Roundup](https://boristhebrave.com/2013/07/14/tileset-roundup)" (2013) | read 2026-10-30 | — | tile counts of corner, edge and blob sets, with and without turns |
| [TileMapDual](https://github.com/pablogila/TileMapDual) README (after O. Stålberg's dual grid and jess::codes's video) | checked 2026-10-30 | MIT | the dual grid: 15–16 tiles instead of 47 |
| Box2D 3.1.1 (`third_party/box2d`: `include/box2d/types.h`, `collision.h`, `math_functions.h`, `src/manifold.c`); [3.2.0](https://github.com/erincatto/box2d/releases/tag/v3.2.0) `include/box2d/types.h` and [release notes](https://box2d.org/documentation/md_release__notes__v320.html); E. Catto, "[Ghost Collisions](https://box2d.org/posts/2020/06/ghost-collisions/)" (2020-06-21), "[Box2D 3.2](https://box2d.org/posts/2026/10/box2d-3.2/)" (2026-10-18) | 3.1.1 `8c661469`; 3.2.0 released 2026-10-19; read 2026-10-30 | MIT | chain shapes, ghost vertices, surface materials, pre-solve, length units, what 3.2 changes |
| [Parry](https://docs.rs/parry3d/latest/parry3d/shape/struct.Voxels.html) `Voxels` (documentation) | checked 2026-10-30 | Apache-2.0 | a grid shape that handles internal edges |
| Unity 6 manuals (product): [Tilemap Renderer modes](https://docs.unity.com/en-us/engine/6000.7/script-reference/unityengine/tilemaps/tilemaprenderer/mode), [Composite Collider 2D](https://docs.unity3d.com/Manual/class-CompositeCollider2D.html), [2D Renderer Data](https://docs.unity.com/en-us/engine/6000.7/manual/unity2d/2d-urp/2drenderer-data-overview), [Pixel Perfect Camera](https://docs.unity.com/en-us/engine/6000.0/manual/unity2d/2d-urp/2d-pixelperfect/ref), [Rule Tile](https://docs.unity3d.com/Packages/com.unity.2d.tilemap.extras@4.0/manual/RuleTile.html) (2D Tilemap Extras 4.0.2); [issue: tilemaps after a Collab merge](https://issuetracker.unity3d.com/issues/tilemaps-arent-updated-correctly-and-sometimes-display-random-tiles-from-the-palette-when-downloaded-from-collab) | checked 2026-10-30 | proprietary (**not read as code**) | chunked tile rendering, outline colliders, light textures, the pixel-perfect camera, rule tiles, a merge bug |
| M. DesLauriers, "[2D Pixel Perfect Shadows](https://github.com/mattdesl/lwjgl-basics/wiki/2D-Pixel-Perfect-Shadows)" (lwjgl-basics wiki) | read 2026-10-30 | — | an occlusion map turned polar into a 1D shadow map |
| G. Rong, T.-S. Tan, "Jump Flooding in GPU with Applications to Voronoi Diagram and Distance Transform", I3D 2006; A. Sannikov, "Radiance Cascades" (2023); J. McGhee, "[Building Real-Time Global Illumination](https://jason.today/gi)" (two parts) | read 2026-10-30 | — | distance fields of occluders, 2D global illumination |
| J. Jylänki, "A Thousand Ways to Pack the Bin" (2010) and [RectangleBinPack](https://github.com/juj/RectangleBinPack) | checked 2026-10-30 | public domain | MaxRects, Skyline, Guillotine, Shelf |
| YellowAfterlife, "[GameMaker: Smooth pixel-perfect camera](https://yal.cc/gamemaker-smooth-pixel-perfect-camera/)" (2020-12-12); G. Sassone, "[Pixel Art Filtering](https://jorenjoestar.github.io/post/pixel_art_filtering)" (2021-04-12) | read 2026-10-30 | ideas only (no code taken) | sub-pixel camera motion on a pixel grid; filters for non-integer scales |
| [Aseprite file format](https://github.com/aseprite/aseprite/blob/main/docs/ase-file-specs.md); crates [asefile](https://crates.io/crates/asefile) 0.3.8 (2024-03-14), [aseprite-loader](https://crates.io/crates/aseprite-loader) 0.4.2 (2026-02-19) | checked 2026-10-30 | the specification states no licence; the crates MIT and MIT OR Apache-2.0 | frames with durations, tags, slices with nine-slice centres and pivots, user data, zlib cels |

## How they work

### Drawing a tile map

- **Godot 4.7 (`TileMapLayer`):**
  - cells live in a hash map keyed by coordinates; a layer is grouped into rendering
    quadrants of 16 × 16 cells (`rendering_quadrant_size`), each one or more canvas items
    (a new one whenever a tile's material or z-index changes);
  - each tile is a rectangle command (`draw_rect_region`) in its quadrant's canvas item,
    so every visible tile becomes a 128-byte instance in every frame the canvas renderer
    draws (ADR 0015's note on Godot);
  - an animated tile adds one command per frame of its animation, each marked by
    `canvas_item_add_animation_slice` so the renderer draws the current one; a cell's
    animation is offset by a hash of its position;
  - a y-sorted layer ignores the quadrant size and groups tiles by their y;
  - edits are batched to the end of the frame and rebuild the dirty quadrants;
  - saved as `tile_map_data`, a byte array: a 2-byte version, then 12 bytes a cell
    (x, y, source, atlas x, atlas y, alternative), base64 in the scene's text since 4.3.
    A user's layer was a 590,000-character line that no merge could handle (forum, 4.5.1).
- **Bevy (main, 0.20) `TilemapChunk`:**
  - a chunk is one quad mesh; its material holds the tile set as a 2D **array** texture (a
    tile a layer, so mips never mix tiles) and a `u16 × 4` data texture of the chunk's
    cells (tile index, colour, visible, flips and a diagonal flip);
  - the fragment shader finds the cell from the quad's coordinate, discards empty cells,
    mirrors the coordinate inside the tile and samples the array's layer;
  - an edit packs the whole chunk into a new vector and uploads the image again;
  - the in-tile coordinate is `fract` of the cell coordinate, so implicit derivatives jump
    at tile borders: with a mip chain the border pixels would take the smallest mip (fine
    for nearest filtering without mips).
- **bevy_ecs_tilemap 0.20:**
  - an entity per tile ("editing tiles is super easy");
  - chunks of 64 × 64 tiles meshed with four vertices a tile, rebuilt when a tile changes;
  - animation in the vertex shader (first and last frame and speed in the vertex);
  - square, isometric (diamond, staggered) and hexagonal (rows, columns, even, odd) grids
    by shader variants; texture arrays from KTX2 or DDS containers or a list of images, or
    an atlas behind a feature; y-sorting per chunk.
- **Phaser 4.1 `TilemapGPULayer`:**
  - a whole layer is one quad, a 32-bit texel a cell (two flips, an animation flag, a
    28-bit tile index) and a second texture for animations;
  - a fixed cost per screen pixel, "no performance loss when many tiles are visible";
    seams under linear filtering handled in the shader;
  - limits: one tile set, orthographic grids only, 4,096 × 4,096 cells; an edit means
    generating the layer's data texture again.
- **Unity 6 (product):** the Tilemap Renderer's Chunk mode groups tiles by place and
  texture and batches them (its fastest, but sprites of one chunk should share an atlas);
  Individual mode sorts each tile alone, to sort with other objects; SRP Batch mode feeds
  Unity's batcher.
- **The technique** (B. Jones, 2012): the map as a small texture whose red and green name
  a tile's column and row in the sprite sheet, one quad, and a fragment shader that looks
  up its texel and then samples the tile; linear filtering would need a gutter around each
  tile, which the demo leaves out, and the view's offset snaps to whole pixels against
  seams.

Side by side, for a layer of N visible tiles covering P pixels:

| | CPU per frame | GPU per frame | An edit | Mips across tiles | Sorting with sprites |
|---|---|---|---|---|---|
| Godot: a rectangle a tile | N instances built | N quads | the quadrant rebuilt | padding (below) | by item, y-sort per tile |
| bevy_ecs_tilemap: a mesh a chunk | nothing | 4N vertices | the chunk meshed | array texture | per chunk |
| Bevy `TilemapChunk`, Phaser: a quad a chunk or layer | nothing | P lookups | the chunk's or layer's data | array texture (Bevy), shader (Phaser) | the chunk as one item |

### Auto-tiling

- **What a tile set can say** (Boris the Brave's classification, 2021): a tile matches its
  neighbours by its **corners** (16 tiles for two terrains; 6 with turns), by its
  **sides** (16), or by **both** (the 256 combinations, of which the "blob" set keeps 47:
  a corner counts only where both of its sides do; 16 with turns and mirrors).
- **Godot 4.7, terrains:**
  - a terrain set has a mode: Match Corners and Sides, Match Corners or Match Sides (the
    heirs of Godot 3's 2 × 2, 3 × 3 and 3 × 3 minimal bitmasks); each tile names its
    terrain and, for each of its neighbours, the terrain across that border ("peering
    bits", −1 for empty);
  - **the map stores tiles only.** Painting a terrain is a greedy solver
    (`terrain_fill_constraints`): for each painted cell in turn, every terrain pattern of
    the set is scored against a set of constraints (the centre and each bit, weighted by
    priority), the lowest score wins, and its bits join the constraints for the next cells.
    The cell's present terrain is read back from its tile. Red-black trees and hash maps on
    the way; the result depends on the order of the cells;
  - Connect (join with the layer's tiles) and Path (join only within the stroke) modes;
    probabilities choose among equal tiles;
  - Better Terrain exists because the built-in system "has some tricky behaviors … It's
    also quite slow, and the API is difficult to use at runtime"; a forum answer explains
    wrong tiles by sheets drawn for a dual grid, which Godot's bits cannot express.
- **Tiled 1.12 (manual only):**
  - corner, edge and mixed sets (16, 16 and 256 tiles for two terrains; the 47-tile blob
    works in a mixed set); corners and edges painted on the tiles in the tileset editor; a
    Patterns view darkens the combinations that exist, so the missing ones show;
  - the terrain brush paints corners or edges and fixes the neighbours; a missing direct
    transition goes through an intermediate terrain (dirt onto cobblestone through sand);
  - probabilities: a tile's own times its corners' terrains'; 0 keeps a tile out of
    automatic use;
  - flipped and turned tiles count as variations (the blob's 47 become 15);
  - all tiles of a terrain set in one tileset; one layer at a time; Automapping (rule
    maps) for whatever terrains cannot say.
- **LDtk 1.5.3, auto-layer rules:**
  - an IntGrid layer stores small integers a cell (`intGridCsv`, 0 empty); its rules (or a
    separate auto-layer's, reading another layer) paint tiles;
  - a rule is a square pattern of odd size, up to 9 × 9, whose cells say: value N
    required, −N forbidden, any value, no value, or a group of values; cells outside the
    level read an out-of-bounds value;
  - a rule passes a chance (a hash of the layer's seed plus the rule's id and the cell, so
    the same every time), an optional Perlin filter, a modulo in x and y with offsets or a
    checker; it is tried as drawn and mirrored in x, y or both;
  - its output is one of several tile rectangles chosen at random, or a stamp with a
    pivot, with optional random offsets;
  - rules run in their groups' order; **break on match** (on by default) stops later rules
    in a cell where a rule matched, and an opaque tile hides what lies under it;
  - an edit re-runs every rule over the edited rectangle grown by half the largest pattern;
  - **the file keeps both** the IntGrid and the made tiles (`autoLayerTiles`, in drawing
    order, each with the rule and cell that made it), so a game's loader needs no rules;
  - the Rules Assistant (1.2) makes a rule group from a layout of "wall fragments" (full,
    single, sides, inner and outer corners, T-walls, ends, diagonals) that the user fills
    with tiles; optional rule groups per level; biomes.
- **Unity's Rule Tile (product):** a tile asset with rules of a 3 × 3 (or extended)
  neighbourhood: This, Not This, Don't Care, against the same Rule Tile; each rule fixed,
  rotated or mirrored; its output fixed, random (with noise) or animated; the first
  matching rule wins, top to bottom. The map stores the rule tile; the sprite follows from
  the neighbours when the cell is refreshed.
- **The dual grid** (O. Stålberg; jess::codes; TileMapDual, MIT): the user paints cells; the
  tiles are drawn on a grid offset by half a cell, each from the four painted cells at its
  corners. Two terrains need 16 tiles (15 and an empty one), or 6 drawn by hand when
  turns and mirrors are allowed. No solver: each drawn tile is a function of four cells.

### Atlases

- **ADR 0015:** every quad names its bindless texture, so atlases buy no batching.
- **Godot:** a tile set source is one image cut by a grid (margins, separation, region
  size), tiles made automatically in non-transparent cells; **Use Texture Padding** (on by
  default) adds a transparent pixel around each tile against bleeding when filtering is on;
  an atlas-merging tool combines sources.
- **Unity (product):** Sprite Atlases pack sprites into shared textures for its batching
  (padding, rotation, tight packing, variants).
- **Bevy:** `TextureAtlasBuilder` packs with `rectangle-pack`, `DynamicTextureAtlasBuilder`
  with `guillotiere`; its tile chunks use array textures instead.
- **Packing:** Jylänki's survey (2010; public-domain code) compares MaxRects, Skyline,
  Guillotine and Shelf; MaxRects with best-short-side fit packs tightest of the fast ones.
- **What an atlas still buys without batching:** fewer textures (each with its allocation
  and its descriptor), and fewer files to stream. What it costs: a packing step, settings
  on every image, mips that mix neighbouring sprites unless padded, and a cook that must
  re-pack when one sprite changes.

### 2D lights and shadows

- **Godot 4.7:**
  - point and directional lights, occluders as polygons; up to 256 lights a render;
  - each light with shadows draws the occluders' polygons into two rows of one `R32F`
    shadow texture (`rendering/2d/shadow_atlas/size` wide), in four 90° sections, one
    draw per occluder and section: a one-dimensional depth map around the light;
  - a canvas item is lit by at most 15 lights whose rectangles, masks and z ranges it
    meets, listed in its instance as 8-bit indices; the canvas shader loops over them and
    filters the shadow (none, PCF5, PCF13); normal and specular maps come with the item's
    texture;
  - a distance field of the occluders (jump flooding, `canvas_sdf.glsl`) is made only
    when a shader reads it (`texture_sdf`, particles' collisions).
- **Unity's 2D renderer (product):** lights are drawn into light textures before the
  sprites, at a render scale (0.5 of the screen by default), up to four textures (blend
  styles); sprites read them; shadow casters; normal maps.
- **M. DesLauriers:** occluders drawn into an occlusion map around the light, turned polar
  into a one-dimensional map of the nearest occluder per angle (256 rays in the example),
  then the light drawn with a blur; "generally less performant than geometry shadows"
  because of the fill rate and passes.
- **bevy_light_2d 0.11:** a full-screen distance field of rectangle occluders (each pixel
  against each occluder; 128 at most under WebGL2), then for every pixel and light a ray
  march of up to 48 steps towards the light, then the scene multiplied by the light map.
- **Global illumination in 2D:** a distance field by jump flooding (Rong and Tan, 2006),
  rays marched through it (McGhee's articles), and radiance cascades (Sannikov, 2023, used
  by Path of Exile 2) for noiseless bounce light; several full passes a frame.

### Tile collision

- **Ghost collisions** (Catto, 2020): a box sliding over a row of boxes catches on the
  inner corners between them. Box2D's answer is the chain shape, whose segments know their
  neighbours (ghost vertices), so a contact normal pointing into an inner joint is
  skipped. Chains are one-sided: "smooth collision can only be efficiently and robustly
  achieved with one-sided edge chains", since telling inside from outside on a two-sided
  loop is a point-in-polygon test over hundreds of edges.
- **Box2D 3.1.1, vendored:** `b2ChainDef`: at least four points, counter-clockwise (the
  normal on the right of the segment's direction), a loop or open (an open chain's first
  and last segments do not collide), static bodies only, points farther apart than the
  linear slop (5 mm), no self-intersection; a `b2SurfaceMaterial` per segment (friction,
  restitution, rolling resistance, tangent speed, and a user material id passed to
  queries); polygons of at most 8 vertices; the pre-solve callback
  (`bool (*)(shapeA, shapeB, b2Manifold*, ctx)`, thread-safe, awake dynamic bodies only,
  not sensors); `b2SetLengthUnitsPerMeter` global, set before any other call.
- **Box2D 3.2.0 (2026-10-19):** chains "segment based": `pointCount`, an open chain
  takes explicit ghost points (`ghost1`, `ghost2`) and every one of its segments collides,
  "you may overlap two open chains using the ghost points to get smooth collision";
  materials one or one a segment; the pre-solve callback returns nothing and turns a
  contact off by setting the manifold's point count to zero, with a separate callback for
  continuous collision; continuous sensor events; optional double-precision positions;
  recording, replay and world snapshots (experimental); about three times 3.1's speed by
  Catto's post.
- **Godot 4.5 and later:** physics quadrants of 16 × 16 cells; in each, one static body per
  physics layer, constant velocity, one-way setting and, for one-way tiles only, row (so
  one-way tiles never merge vertically); the quadrant's polygons are merged
  (`Geometry2D::merge_many_polygons`) and decomposed back into convex shapes. Seams remain
  between the convex pieces and between quadrants. Users noticed that
  `get_coords_for_body_rid` now names the quadrant, not the tile (forum).
- **Unity (product):** the Tilemap Collider 2D with a Composite Collider 2D merges the
  tiles into Outlines (hollow, as Edge Colliders) or Polygons; with Outlines, an overlap
  test inside a solid area finds nothing, because outlines have no inside (forum).
- **Parry's `Voxels`** (Rapier): a grid shape that handles the internal edges itself;
  experimental (voxels against voxels not supported).

### Pixel-perfect cameras

- **Ours already:** a fixed `render_size` scaled by whole numbers, nearest, centred.
- **Godot:** stretch mode `viewport` (render at the base size, then scale) with scale mode
  `integer` (4.2): 640 × 360 scales to 1280 × 720, 1920 × 1080, 2560 × 1440 and 3840 × 2160
  without bars; `canvas_items` mode for sprites that move by fractions of a pixel.
- **Unity's Pixel Perfect Camera (product):** the assets' pixels per unit, a reference
  resolution, a crop frame (pillarbox, letterbox, windowbox, stretch fill), grid snapping
  (pixel snapping, or rendering into a texture of the reference size and scaling it), and
  point or "Retro AA" filtering for stretch fill.
- **Smooth motion on a pixel grid** (YellowAfterlife, 2020): render the view one pixel
  larger, keep the camera on whole pixels, and shift the scaled image by the fraction of
  the camera's position times the scale; parallax layers that scroll at other rates are
  drawn apart, or they jitter.
- **Non-integer scales** (Sassone, 2021, comparing nine filters): nearest inside each texel
  and a blend about one screen pixel wide at texel edges, sized by `fwidth`; plain
  bilinear smears every pixel.

## What we take (proposed)

1. **2D in the one world** (Unity's and Bevy's way, not Godot's `Node2D`): a 2D entity is an
   entity whose `Transform` lies in the XY plane; a 2D view is an orthographic camera.
   Everything built for entities works for sprites unchanged (ADR 0125, question 1).
2. **Metres as world units,** an image's pixels per unit for its size; Box2D at its own
   tuning (question 2).
3. **A flat tile layer as a quad a chunk with a lookup** (Bevy's `TilemapChunk`, Phaser's
   `TilemapGPULayer`, Jones's technique), with what they lack:
   - cells as `u32` in chunks of 32 × 32 (4 KiB) in one GPU buffer reached by address:
     the tile's index in the tile set's table (20 bits), its eight orientations (two flips
     and a transpose, 3 bits), 9 bits spare; an edit writes 4 bytes and uploads its chunk
     once a frame;
   - the tile set's table gives each tile its bindless image and rectangle, so a tile set
     may draw from any number of images (Phaser allows one, Bevy one array);
   - the sample takes the gradients of the continuous map coordinate, not of the in-tile
     `fract`, so mip levels do not jump at tile borders (Bevy's shader would);
   - animated tiles change their table entry on the CPU when their frame changes, so the
     shader knows no time; a tile set may give an animation several phase-shifted entries
     that cells pick by a hash of their place (Godot's per-cell offset, without time in
     the shader);
   - the chunks in view are a range computed from the view's rectangle: no per-chunk test;
   - a layer of tiles taller than their cell, or sorted by y with characters, is drawn as
     quads in the canvas instead, each keyed by its base's y.
4. **Cells in the scene, by rows of chunks through a palette** (question 3): what the
   others do shows the cost of each choice. Godot's one binary line cannot be merged;
   Unity's index tables broke tiles after merges ("won't fix"); LDtk's CSV array and Tiled's
   CSV rows are text but not per chunk.
5. **Terrain layers store terrains; tiles are made from them** (question 4), as LDtk's
   IntGrid and Unity's Rule Tiles do, not stored as Godot and Tiled store them; the cook
   stores the made tiles in packs (LDtk's cached `autoLayerTiles`, at cook time instead of
   in the source).
6. **Terrains as pure functions of stored terrains** (question 5): a **blob** mode (a cell's
   tile from its eight neighbours being of its terrain or not: 47 tiles, 16 with turns) and
   a **corner** mode (the dual grid: a tile from the four cells at its corners: 16 tiles for
   two terrains, 6 with turns), both looked up per cell with no solver; a random choice is
   a hash of the cell, the terrain set and the map's seed (LDtk's seeded hash), so an edit
   never reshuffles untouched cells; layout templates fill a sheet's bits in one command
   (LDtk's Rules Assistant, GameMaker's 16- and 47-tile templates). Then LDtk's rules for
   decoration: patterns over terrains, chance, flips, modulo, break on match, ordered
   groups.
7. **Tile collision as chain loops** (Box2D's answer to ghost collisions; Unity's
   Outlines), with what Godot and Unity lack:
   - the solid region's outline traced from the cells: full tiles by the grid's edges
     between solid and empty cells; shaped tiles by **edge cancellation**: every shape's
     edges in the layer's integer pixels, collinear overlaps split at their ends, opposite
     pairs cancelled, the rest linked into loops and collinear corners dropped. Exact in
     integers as long as a tile's shapes stay inside its cell and do not overlap (the tile
     set editor clips and unions them when a tile is saved);
   - **per chunk of 32 × 32 cells**: loops inside a chunk stay loops; a loop crossing a
     border becomes open chains whose ghost points are the neighbouring chunk's points
     (Box2D 3.2's explicit ghosts), so collision stays smooth across chunks and an edit
     rebuilds one chunk's chains (two when it touches a border);
   - a segment's surface material from its tile (friction, restitution, and the user
     material id that queries return, for footsteps by surface);
   - one-way tiles merged per row only (Godot's rule), as open chains with a pre-solve
     filter;
   - chains are hollow: whether a point is inside solid tiles is answered by the grid,
     never by a Box2D overlap (Unity's Outlines lesson);
   - **Box2D 3.2 first:** its open chains with explicit ghost points are what the per-chunk
     split needs, and its pre-solve and chain definitions changed; the shim's new calls
     are written once, against 3.2.
8. **No atlases for batching;** padding (Godot's transparent pixel, or the edge pixels
   repeated) at cook only where mips would mix tiles; texture arrays and MaxRects packing
   only where measurement shows they pay.
9. **2D lights:** shadows as Godot draws them (occluder geometry into a one-dimensional
   depth strip per light: no pass of its own over the screen, each light paid over its own
   pixels) measured against Unity's light textures (each light drawn once at reduced
   resolution, one read a pixel); the estimates favour the strips for small lights and
   light textures for many large ones; lit sprites their own pipelines, made only when a
   view has a light; occluders from tile sets per tile; a distance field (Godot's jump
   flooding, made only when read) left for 2D global illumination (question 9).
10. **Pixel art:** the reference size and integer scaling we have; sub-pixel smoothing by
    rendering one pixel larger and shifting (YellowAfterlife) and a sharp-bilinear filter
    for fractional scales later (E13-7); 2D views default to a neutral output, so a
    sprite's colour reaches the screen as painted (none of our tone curves does that).

## What we do not take, and why

- **A rectangle a tile every frame** (Godot): cost grows with the tiles in view; ours with
  the pixels covered.
- **An entity a tile** (bevy_ecs_tilemap): 65,536 entities for one 256 × 256 layer, each
  with ECS overhead, for data that is a grid.
- **Godot's terrain solver over stored tiles:** order-dependent, slow, and unable to update
  maps when a terrain set changes.
- **A layer's cells as one binary line** (Godot) **or index tables merged silently**
  (Unity): CLAUDE.md §13 asks for text that merges.
- **The made tiles kept in the source file** (LDtk): two copies that disagree after a
  merge until regenerated; the cook keeps them instead.
- **Merging tiles into convex polygons** (Godot's quadrants): seams remain between pieces.
- **A Box2D overlap test for "inside a wall"** with chains: always false inside.
- **A full-screen distance field per frame for lights** (bevy_light_2d: every pixel
  against every occluder, then up to 48 steps per pixel and light) and **the polar
  transform** (DesLauriers): fill rate the Radeon does not have (estimates below).
- **Texture arrays for tile sets now** (Bevy): one size per array and a copy of the sheet;
  the bindless table already avoids atlases. Kept as the cure if padded mips are not enough.
- **One tile set a layer and a whole-layer data texture** (Phaser): our chunks and table
  remove both limits.

## Measurements before the ADR (estimates; measured in each step)

The per-pixel costs come from ADR 0015's 8-pixel sprites, whose vertex work per pixel is
far larger than a full-screen quad's, so they bound the cost from above; the Radeon's upper
figure (0.57 ns a pixel) stands in for the scene colour's twice-as-wide pixels.

| Case | RTX 4060 Ti | Radeon 2-CU |
|---|---:|---:|
| A full-screen flat tile layer at 1080p (2.07 Mpx; the lookup's extra read counted at +30 %) | about 0.07–0.1 ms | about 1.0–1.5 ms |
| A 256 × 256 map, 16-px tiles at 3× (920 cells in view, at most 6 chunks a layer): two layers and 900 sprites of 48 px at 1080p | about 0.2–0.25 ms | about 3–4 ms |
| The same map at a pixel-art 640 × 360, scaled 3× | about 0.02–0.03 ms | about 0.3–0.45 ms |
| The map zoomed out whole (1.17 Mpx a layer), a quad a chunk | about 0.04 ms a layer | about 0.55–0.85 ms a layer |
| The same as a quad a tile (65,536 a layer) | about 0.1 ms a layer, plus about 1 ms of CPU | about 1.5–2.2 ms a layer, plus about 1 ms of CPU |

- **CPU, a quad a tile:** 65,536 quads cost 0.45 ms to record (7 ns each) and 0.5–0.75 ms
  to prepare and upload (ADR 0015's 1.14 and 0.76 ms per 100,000), every frame; a quad a
  chunk costs nothing per tile.
- **Lights, 8 with shadows, a full screen of lit sprites with normal maps at 1080p, on the
  Radeon:**
  - the worst case, every light covering the whole screen: shadow strips read up to 40
    texels a pixel (8 lights, five taps) in the sprites' shader, five to ten times an unlit
    sprite's cost: about 4–10 ms; light textures pay a pass for the normals (about 1 ms),
    each light at a quarter of the pixels with its shadow (about 0.3–0.4 ms a light) and
    one read a pixel: about 3–5 ms;
  - torch-sized lights (a twentieth of the screen each): the strips add about two reads a
    pixel on average, about 0.2–0.5 ms; light textures still pay the normals' pass, about
    1.1–1.6 ms;
  - at 640 × 360, about a ninth of each; the RTX about a fifteenth to a twentieth;
  - a distance field per frame (jump flooding at 1080p: about eleven passes) and a 48-step
    march per pixel and light are beyond the Radeon's budget at 1080p.

  These are estimates from the per-pixel costs above; E13-6 measures both kinds of shadows
  on both GPUs, with small and large lights.
- **Collision:** tracing a 32 × 32 chunk's outline touches 1,024 cells (microseconds); Box2D
  creates a shape per segment, so a 256 × 256 layer of rough terrain (thousands of
  segments) is built at load, and an edit replaces one chunk's few hundred segments.
  Measured in E13-5 (outline, chain creation, and the step's cost against boxes per tile).

## Patents (checked 2026-10-30)

- **Choosing tiles by matching neighbours** (terrains, blob and corner sets, rules): a
  search found no patent on it; matching tiles by their edges is Wang's (1961), and the
  tile editors above have shipped it for years. Cohen et al.'s Wang tiles for texture
  synthesis (2003) are a different use (non-periodic textures), not what we do.
- **Shadows in 2D** from a one-dimensional depth map are shadow mapping (Williams, 1978)
  around a point; nothing found.
- **Jump flooding and radiance cascades** are not used in E13; their patents are to be
  checked before a step uses them.

## Questions for the team

ADR 0125's questions 1 to 10 merge this note's with the UX note's; each there has its
context, options with their costs, and a recommendation.
