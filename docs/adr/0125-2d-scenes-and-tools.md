# ADR 0125: 2D scenes, tile maps and the editor's 2D tools (E13)

- Status: **Accepted** (2026-10-30, team decisions 1а 2а 3а 4а 5в 6а 7а 8а 9а 10а: every
  question as recommended; 9в, soft shadows and bounced light, later as a step of its own).
  The questions at the end keep their options for the record.
- Research: [2D scenes' runtime](../research/2d-scenes.md) (tile map renderers, auto-tiling,
  atlases, 2D lights, tile collision for Box2D, pixel-perfect cameras) and [the 2D tools'
  UX](../research/ux/2d.md) (Godot, Unity, LDtk, Tiled, Aseprite, Paper2D).
- Builds on [ADR 0015](0015-renderer-2d.md) (the 2D renderer), [ADR 0016](0016-viewports.md)
  (viewports, pixel-perfect scaling), [ADR 0028](0028-physics-scope.md) (Box2D v3),
  [ADR 0066](0066-physics-queries-characters-rollback.md) (2D queries, the 2D mover),
  [ADR 0072](0072-frame-renderer.md) (the frame renderer), [ADR 0104](0104-save-games.md)
  (saves), [ADR 0107](0107-editor.md) §7 (phase E13), [ADR 0108](0108-scene-format-2.md)
  (assets by id and path, parts, `.meta`), [ADR 0109](0109-viewport-picking.md) (picking),
  [ADR 0110](0110-blockout-brushes.md) (Tab's editing, colliders made from what is drawn),
  [ADR 0112](0112-asset-previews.md) (previews), [ADR 0114](0114-luau-api.md) (scripts),
  [ADR 0121](0121-mixer-file.md) (a settings file named by the player's settings),
  [ADR 0123](0123-material-graphs.md) (the editor's file grammar) and
  [ADR 0124](0124-animation-graphs.md) (the `Animator`, clips, events, graphs).

## Context

- **What exists:**
  - `engine_render2d` (ADR 0015): a `Canvas` of 64-byte quads (sprites with hard edges, SDF
    rectangles, circles, capsules and outlines), each carrying its bindless texture, so
    texture changes never split a batch; a deterministic order (layer, then a key such as
    y, then submission order) by a radix sort that allocates nothing; `Camera2d` and
    `View2d`; glyph quads for text. On ref-1, 100,000 8-pixel sprites take 164 µs on the
    RTX and 2.5 ms on the Radeon, and 7 ns a quad to record on the CPU.
  - Viewports (ADR 0016) with pixel-perfect integer scaling; the frame renderer (ADR 0072)
    with a fixed `render_size` and `Upscale::Integer` (nearest, centred).
  - `engine_physics2d` (ADR 0028, ADR 0066): Box2D v3.1.1 behind a C shim with box, circle
    and capsule shapes on 64 query layers, casts, overlaps and a capsule mover. Box2D 3.2.0
    came out on 2026-10-19: chains with explicit ghost points, a new pre-solve callback,
    about three times 3.1's speed by its author's post.
- **What does not:**
  - No 2D component in scenes: the player draws the game's UI canvases (`Game::ui`) and
    nothing else of `engine_render2d`; `engine_physics2d` is used by nothing outside its
    crate.
  - The scene's `Camera` is perspective only (`fov_y`). Culling and the light tiles accept
    an orthographic projection already, but particles skip orthographic views (ADR 0081),
    and the cascades and AO were built and tested for perspective views only.
  - None of the tone curves leaves a colour as it was painted, which pixel art expects.
  - No tile maps, tile sets, sprite slicing, atlases, flipbooks, 2D lights or 2D view in
    the editor. The texture pool makes 2D images only (one layer), though the shaders
    already alias `Texture2DArray` and `TextureCube` on the bindless binding.
- **E13's row** (ADR 0107 §7): tile maps (painting, rules), sprites (slicing, atlases), the
  2D view's tools, 2D physics shapes. **CLAUDE.md §6** asks of the 2D renderer: bindless
  batching without atlas juggling, sprites, 9-slice, chunked tile maps with GPU culling,
  particles, SDF primitives, 2D lights and shadows with normal maps, custom Slang
  shaders, layers, y-sort and a user key, deterministic.
- **The rules:** speed first and measured on both GPUs; no allocation per frame; nothing
  paid where unused (a 3D game pays nothing for 2D, an unlit 2D game nothing for lights);
  text source files that merge, with stable ids (CLAUDE.md §13); the editor's §12
  principles.

## Options

### Where 2D lives (question 1)

- **a. One world:** a 2D entity is an entity whose `Transform` lies in the XY plane (y up,
  turned about z); a 2D view is a camera with an orthographic projection looking down −Z.
  Hierarchy, propagation, interpolation, picking, gizmos, prefabs, saves, replication,
  scripts' `Transform`, `.anim` tracks and `JointAttachment`-style children work as they
  are. Unity's and Bevy's way. A transform takes 48 bytes where a 2D one would take 20.
- **b. A 2D transform and 2D-only scenes** (Godot's `Node2D`): smaller, but a second
  propagation, interpolation, gizmo, inspector, script type, network schema and track kind;
  2D inside 3D only through a viewport's texture.

### World units (question 2)

- **a. Metres,** an image's pixels per unit giving its size (Unity's way).
- **b. Pixels** (Godot's and GameMaker's), Box2D told a character's height in pixels.

### How the 2D world is drawn

- **a. By `engine_render2d` inside the view's scene passes**, at the render size, before the
  upscale and the post chain: bloom, exposure, tone mapping and grading can apply to
  sprites as to a 3D scene; pixel art renders at its reference resolution and is scaled by
  integers. **Chosen:** pixel-perfect output, HDR glows where a game wants them, and the
  2D renderer's order and cost.
- **b. After the post chain, as the UI layer is:** no bloom or tone mapping on the world;
  HDR glows lost.
- **c. As quads through the forward renderer** (Unity's sprites are meshes in its
  pipeline): depth-sorted with 3D, but every sprite pays the 3D path (per-instance
  culling, the forward shading's registers) and loses the 2D renderer's deterministic
  order.

### How a tile map is drawn (the research note's measurements and sources)

- **a. A quad per tile** through the canvas: simplest; the cost grows with the tiles in
  view (65,536 quads for a 256 × 256 map zoomed out: about 1.5–2.2 ms of GPU a layer on
  the Radeon and about 1 ms of CPU to record, prepare and upload).
- **b. A mesh per chunk** (bevy_ecs_tilemap): no per-frame recording, but vertices per tile
  still, and a chunk rebuilt on every edit.
- **c. A quad per chunk** whose fragment shader finds its cell's tile in the chunk's cells
  and samples the tile set (Bevy's `TilemapChunk`, Phaser's `TilemapGPULayer`): the cost
  follows the pixels covered, not the tiles; an edit writes 4 bytes. Only tiles that fit
  their cell and need no sorting can be drawn so. **Chosen, with a for the rest.**

### Where a tile map's cells are kept (question 3)

- **a. In the scene, by rows of chunks through a palette;** **b. in a file of their own
  beside the scene;** **c. binary in the scene** (Godot's one line).

### What a terrain layer stores (question 4)

- **a. The terrains, tiles made from them;** **b. the tiles the brush made** (Godot,
  Tiled); **c. both** (LDtk's file).

### Auto-tiling (question 5)

- **a. Terrains on tiles** (Godot's peering bits, Tiled's terrain sets): each tile says
  which terrain its corners and sides are; the tile for a cell is the one whose marks
  match its neighbourhood.
- **b. Rules** (LDtk's auto-layers, Unity's Rule Tile): patterns of neighbour conditions
  that place tiles, with chance, flips and stacking.
- **c. Both:** a for transitions, b for decoration and what marks cannot say.

### How tile maps collide (the research note)

- **a. A shape per tile** (Godot before its physics quadrants): bodies catch on the seams
  between tiles (ghost collisions).
- **b. Merged outlines:** the solid cells' outlines traced into loops and given to Box2D as
  chain shapes, which are smooth across their vertices (Box2D's own answer to ghost
  collisions; Unity's composite Outlines). **Chosen.**
- **c. Merged polygons** (greedy rectangles, or Godot's merge then convex decomposition):
  fewer shapes than a, seams still between the pieces.

### Atlases

- **a. Images as they are,** each its own bindless texture: no packing step; batching does
  not need atlases (ADR 0015). **Chosen;** packing only where measured to pay.
- **b. Every sprite packed into atlases at import** (Unity's Sprite Atlas, Godot's atlas
  sources): fewer textures; a packing step and its settings on every image.

### Flipbooks (question 6)

- **a. The `Animator` plays sprite clips** (frame ranges with durations: an Aseprite file's
  tags, or ranges named in a sheet's `.meta`), as it plays a model's clips; `.anim` files
  key `Sprite.frame`.
- **b. A component of its own** (Godot's `AnimatedSprite2D` and `SpriteFrames`): a second
  animation system without graphs, events or saves of its own.

### Where sprite settings live (question 7)

- **a. In the image's `.meta`,** slices as parts of the image; **b. a file per sprite and
  per flipbook** (Paper2D, Godot's `SpriteFrames` resources).

### Grid shapes (question 8)

- **a. Square only in E13;** **b. square and isometric;** **c. square, isometric and
  hexagonal.**

### 2D lights (question 9)

How far E13 goes is question 9; the shadow technique is settled by measurement:

- **a. Shadow strips per light** (Godot): occluder edges drawn into a one-dimensional depth
  texture per light; lit sprites read the lights over them.
- **b. Light textures** (Unity's 2D renderer): lights drawn into screen-space textures at a
  reduced resolution, sprites multiplied by them; normal maps need a normal buffer first.
- **c. A distance field of the occluders** (jump flooding each frame) ray-marched per light:
  soft shadows, a road to 2D global illumination, but about ten full passes a frame.

### Scripts (question 10)

- **a. Tile and 2D physics calls** beside the components' fields; **b. read-only calls in
  E13;** **c. fields only.**

## Decision (proposed)

### 2D in the world (question 1: a)

- A 2D entity is an entity: `Transform` in the XY plane, y up, turned about z, scaled in x
  and y. Its z orders nothing (layers do); it stays 0 unless a game wants it.
- **`Camera` gains an orthographic projection:** `projection` (perspective or
  orthographic), `height` (the world height shown), `reference` (a pixel-art resolution:
  the view renders at that size and is scaled by whole numbers, ADR 0016's rule through the
  frame renderer's `render_size` and `Upscale::Integer`). An orthographic view draws the 2D
  world; the scene passes are skipped for it. 3D models in an orthographic view are not
  drawn in E13; a game that mixes them waits for an ADR of its own.
- **2D views start neutral:** a new `ToneCurve::None` (colours clipped, nothing else), no
  bloom, no grading, exposure 1, so a sprite's colour reaches the screen as it was painted
  (a golden shot checks it exactly); a game turns the post chain on for glows. 2D views use
  no temporal antialiasing (no jitter; sprites keep hard edges, SDF shapes antialias
  themselves) and no frame generation in E13 (no motion vectors for sprites yet).
- **World units are metres** (question 2: a), as everywhere else in the engine, Box2D's
  tuning included (its length unit stays 1). An image says how many of its pixels make a
  metre (`pixels_per_unit`; the project's default 16, so a 16-pixel tile is a metre); the
  2D view and the inspector show pixels beside metres.

### Components

All reflected, saved and seen by scripts as any component; in `engine_app` beside `Model`
where they name assets.

- **`Sprite`:** `image` (an `AssetRef`: an image, one of its slices `hero.png#idle_0`, or an
  Aseprite file); `frame` (which of the image's frames, a number, so `.anim` keys it with
  Step keys under ADR 0124's answer 5b); `color` (linear, straight alpha, HDR allowed);
  `flip_x`, `flip_y`; `size` (none: the slice's pixels over its pixels per unit); `mode`
  (simple, sliced by the slice's nine-slice border, tiled); `layer` and `order`; `hidden`.
  The pivot is the slice's (its `.meta`).
- **`TileMap`** (one layer per entity, as Godot 4.3 split its layers into nodes): `tile_set`
  (a `.tileset`), `grid` (`square` in E13, question 8), its cells and palette (question 3),
  `terrain` (a terrain layer, question 4), `seed`, `layer` and `order`, `sorted` (each tile
  drawn as a sprite in its layer's y order: tall tiles), `collides` and `occludes` (by the
  tile set's shapes), `color`. Layers of one map are sibling entities under a group the
  editor makes.
- **Sorting:** `layer` (`i16`, lower first) and `order` (within the layer). A layer may sort
  by y: the project's 2D layers (names, y-sorting) live in a small `.sorting` file named by
  the player's settings, as `.mixer` is (ADR 0121). A `SortGroup` on an entity makes its
  subtree sort as one sprite at its own y (Unity's Sorting Group: a character's body, sword
  and shadow).
- **`Light2d`** (point, spot, directional, ambient; colour, intensity, radius, falloff,
  shadows on or off, the layers it lights) and **`Occluder2d`** (a polygon; tile sets give
  occluders per tile). Normal maps: an image's companion named in its `.meta`.
- **`Body2d`** (static, kinematic, dynamic; gravity scale, damping, fixed rotation, bullet)
  and **`Collider2d`** (box, circle, capsule, polygon, chain; offset, sensor, friction,
  restitution, density, collision layer by name, one way). A collider without a body is
  static. A sensor tells the entity's scripts `on_touch` and `on_untouch`, as 3D triggers
  do (ADR 0114 E7-3).

### Files

- **An image's `.meta`** (question 7: a) gains a `sprite` section: pixels per unit, the
  default pivot, the filter (nearest for pixel art; compression `Uncompressed` already
  exists), slices (named rectangles with a pivot, a nine-slice border and an optional
  collision outline) and clips (named frame ranges with durations, loop, once or
  ping-pong, events as a model's clips have them in ADR 0124). Slices are parts:
  `asset("art/hero.png#run_3", <id>)`. Slicing again keeps the names and ids of slices
  whose rectangles stay.
- **Aseprite files** (`.aseprite`, `.ase`) are an asset kind: their frames (visible layers
  merged with their blend modes: the W3C compositing modes, plus Aseprite's addition,
  subtract and divide), frame durations, tags as clips, slices as parts and cel user data
  `event:<name>` as events, read by our own Rust parser of the published format (zlib
  through the `miniz_oxide` already built for `png`; the format is about 550 lines of
  specification, and a parser of our own reads only what we use, where `aseprite-loader`,
  MIT OR Apache-2.0, would be a dependency for one importer). What the editor adds lives in
  their `.meta`.
- **`.tileset`**, in the editor's grammar (ADR 0123's rules: entries keyed by short random
  ids or by what they are, names as labels, defaults left out, sorted on write):

  ```text
  [tileset version=1 id=0b6c…]

  tile = [16, 16]
  pixels_per_unit = 16
  sources["k2"].image = asset("art/forest.png", 5f2c…)
  sources["k2"].margin = [0, 0]
  sources["k2"].gap = [0, 0]
  terrains["wall"].mode = "blob"
  terrains["wall"].color = (0.4, 0.4, 0.45, 1)
  terrains["grass"].mode = "corners"
  terrains["grass"].color = (0.3, 0.7, 0.2, 1)
  terrains["dirt"].mode = "corners"
  terrains["dirt"].color = (0.5, 0.35, 0.2, 1)
  tiles["k2:0,3"].shapes.0 = [0, 0, 16, 0, 16, 16, 0, 16]
  tiles["k2:0,3"].surface = "stone"
  tiles["k2:4,3"].shapes.0 = [0, 0, 16, 0, 16, 8, 0, 8]
  tiles["k2:4,3"].one_way = true
  tiles["k2:1,0"].terrain = ["grass", "grass", "dirt", "grass"]
  tiles["k2:7,1"].frames = ["k2:7,1", "k2:8,1", "k2:9,1"]
  tiles["k2:7,1"].frame_time = 0.15
  tiles["k2:7,1"].phases = 4
  tiles["k2:2,5"].weight = 0.25
  tiles["k2:2,5"].data["footstep"] = "grass"
  ```

  A tile is its source and its cell in the source's grid (`k2:4,3`); a tile with nothing
  but defaults has no line. Shapes are polygons in the tile's pixels, kept inside the
  tile's cell and not overlapping (the tab clips and unions them when a tile is saved).
- **A tile map's cells, in the scene** (question 3: a): chunks of 32 × 32 cells, a line per
  non-empty row of a chunk, through a palette with runs:

  ```text
  [entity 4e1a…]
  Name = "Ground"
  TileMap.tile_set = asset("tiles/forest.tileset", 0b6c…)
  TileMap.palette["a3kq"] = "k2:0,3"
  TileMap.palette["b81z"] = "k2:1,3"
  TileMap.palette["x0d4"] = "k2:1,3 flip_x"
  TileMap.cells["0,0"][14] = ". . a3kq*12 b81z x0d4 . a3kq*16"
  TileMap.cells["0,0"][15] = "a3kq*32"
  ```

  - A palette key is four base-36 characters of a hash of what it names (a tile and its
    orientation, or a terrain), lengthened only when two names in one map share a key, so
    two branches that add the same tile write the same line and git merges them.
  - Empty rows and chunks have no line. A paint stroke changes the rows it crosses; two
    people painting apart touch different lines; a prefab instance overrides single rows
    (ADR 0108's overrides by field path); replacing every grass tile is one palette line.
  - A terrain layer (question 4: a) keeps terrains in the same rows
    (`palette["g7k2"] = "terrain grass"`); a cell may hold a tile instead, which stays
    where it was placed.
  - A save keeps the rows a game changed, by ADR 0104's rule for the level's own objects.

### Drawing

- **The player's 2D pass:** in an orthographic view's scene passes, at its render size,
  before the upscale and the post chain. A view with no 2D content declares no 2D pass.
- **Sprites** are recorded each rendered frame from the world into a `Canvas` the player
  keeps (no allocation in steady state), at their pose between the last two ticks as
  `RenderExtract` interpolates 3D instances, culled by the view's rectangle, keyed by layer,
  y or order and submission order.
- **Tile maps (option c, with a for the rest):**
  - each layer's cells live in chunks of 32 × 32 `u32`: the tile's index in the tile set's
    table (20 bits), its eight orientations (two flips and a transpose, 3 bits), 9 bits
    spare; on the CPU and in one GPU buffer reached by address (4 KiB a chunk); an edit
    writes the cell and uploads its chunk once a frame;
  - the tile set's table gives each tile its bindless image and rectangle (32 bytes; any
    number of source images, no atlas); an animated tile's entries are rewritten on the
    CPU when its frame changes, so the shader knows nothing of time; `phases` gives an
    animation several shifted entries that cells pick by a hash of their place, so water
    does not ripple in step;
  - a flat layer draws one quad per visible chunk, entered in the canvas in its layer's
    place with a fragment entry point of its own in its batch state (ADR 0015's material
    slot): a lookup of the cell, the tile's entry, and a sample with the gradients of the
    continuous map coordinate, so mip levels do not jump at tile borders. Its own
    pipeline: sprites pay no register for it;
  - the visible chunks are a range found from the view's rectangle in the map's space: no
    per-chunk test and no GPU culling (CLAUDE.md §6 asked for GPU culling; on a grid the
    range is exact and costs nothing per hidden chunk);
  - a corner-mode terrain layer is drawn half a cell off its painted grid (the dual grid);
  - a sorted layer (tiles taller than their cell, y-sorted with characters) draws its
    visible tiles as quads in the canvas, each keyed by its base's y.
- **Atlases (option a):** none for batching. The cook packs images only where measurement
  says it pays: many small images (each texture's allocation and descriptor), a tile set
  whose mip levels bleed across tiles when zoomed out (padded at cook, or a texture array
  once the texture pool makes them). Measured in E13-3.

### Tile maps' rules (questions 4, 5)

- **A terrain layer's tiles are made, never stored in the scene:** when the map loads, and
  around each edit (the 3 × 3 cells about a changed cell, or a rule's reach). The cook
  stores the made tiles in packs, so a shipped game runs no terrain code at load; the
  editor makes them in the frame of the edit.
- **Two terrain modes, each a lookup per cell with no solver** (question 5):
  - **blob:** a cell of a blob terrain takes the tile whose marks match which of its eight
    neighbours share its terrain (the 47-tile set; 16 when turned and mirrored tiles are
    allowed);
  - **corners:** the dual grid: each drawn tile is chosen by the four painted cells at its
    corners (16 tiles for two terrains; 6 with turns), any number of terrains meeting;
  - where the tile set lacks a combination, the tile with the fewest mismatched marks
    wins, ties by the terrains' order; the editor lists the missing combinations;
  - a random choice among equal tiles is weighted by their `weight` and decided by a hash of
    the cell, the terrain set and the map's seed, so an edit never reshuffles untouched
    cells and every machine makes the same map.
- **Layout templates** (the 47-tile blob, the 16-tile blob with turns, the 16-tile corner
  set, Godot 3's 3 × 3 minimal) fill the marks of a whole sheet in one command.
- **Decoration rules** come after (E13-4b), LDtk's model over the same stored terrains: a
  square pattern (odd sizes up to 9) of required and forbidden terrains, a chance by the
  same hash, flips, a modulo in x and y, ordered groups, and break on match; tiles placed
  in the same layer or another.

### 2D physics

- **Box2D 3.2 first:** E13-5 begins by updating the vendored copy (3.1.1 → 3.2.0). Its open
  chains with explicit ghost points are what per-chunk collision needs, its pre-solve and
  chain definitions changed, and the shim's new calls are written once, against 3.2. ADR
  0066's tests, its determinism test and `queries2d` are run before and after.
- **`ScenePhysics2d`:** one Box2D world per scene world, stepped in the fixed ticks, moving
  bodies interpolated between ticks as the 3D ones are (ADR 0066), saved and restored as
  `ScenePhysics` is (poses and velocities, ADR 0104), bodies naming their entities for
  queries and contacts.
- **Tile collision (option b):**
  - a layer's solid region is traced from its cells: full tiles by the edges between solid
    and empty cells; shaped tiles by edge cancellation (every shape's edges in the layer's
    integer pixels, collinear overlaps split at their ends, opposite edges cancelled, the
    rest linked into loops, collinear corners dropped): exact in integers, since a tile's
    shapes stay inside its cell and do not overlap;
  - **per chunk of 32 × 32 cells:** loops inside a chunk are chain loops; a loop that
    crosses a chunk's border becomes open chains whose ghost points are the neighbouring
    chunk's points, so collision stays smooth across chunks; all on one static body per
    layer; an edit rebuilds one chunk's chains (two when it touches their border), traced
    off the frame's thread and swapped in at the next tick;
  - each segment's surface material comes from its tile (friction, restitution, and the
    user material id queries return, so a footstep knows it hit grass);
  - one-way tiles merge along their row only (Godot's rule) into open chains with a
    pre-solve filter;
  - chains are hollow: "is this point inside a wall" is answered by the layer's cells, not
    by a Box2D overlap, which finds nothing inside (Unity's Outlines show the same);
  - points closer than Box2D's linear slop (5 mm) are merged: at 16 pixels a metre a pixel
    is 62.5 mm, at 100 it is 10 mm.
- **The shim grows** polygons (up to Box2D's 8 vertices; larger ones split convex), chains,
  segments, sensors, filters and the pre-solve callback. Chains are for static bodies only
  (Box2D's rule); a moving platform of tiles gets polygons.

### 2D lights (question 9: a)

- Lit sprites are their own fragment entry point and pipelines, made only when a view has a
  2D light: an unlit game pays nothing, in registers or passes.
- Point, spot, directional and ambient lights; occluders from polygons and tile sets;
  shadows hard or filtered; normal maps.
- The shadow technique is chosen by measuring shadow strips (Godot's) against light
  textures (Unity's) on both GPUs at 1080p and at 640 × 360, with 8 and 64 lights, small
  (torches) and large (a sun through a window). Prior: the strips where lights are few or
  small (no pass of their own, each light paid only over its own pixels), light textures
  where many large lights overlap (one read a pixel); both stay if each wins its case. A
  distance field and bounce light are a later step with their own ADR.

### The editor's tools

From the UX note:

- **The 2D view:** the viewport turns 2D when the scene's camera is orthographic or by its
  2D switch: orthographic onto XY, the middle button pans, the wheel zooms about the
  pointer in whole-number steps of the art's pixels (no shimmer), Ctrl+0 is 1:1, F frames
  and Ctrl+1..9 keep bookmarks as in 3D; a pixel grid from 800 %, the tile grid, the game
  camera's frame; snapping under the owner's rules (Shift by one pixel of the art, Ctrl to
  the tile grid, G toggles); the gizmo in 2D; sprites picked by their visible pixels and
  outlined. 3D content is not drawn in a 2D view in E13.
- **Sprites:** an image dropped is a sprite at the pointer; a double-click opens the image
  in a tab of its own to slice it (grid by size or count, islands of opaque pixels, by
  hand with a double-click fitting connected pixels; names; pivots by presets; nine-slice
  borders by handles; re-slicing keeps names), and Make Clip (M) makes a clip of the
  frames picked; the image's clips preview in its inspector as a model's do.
- **Tile sets** open in a tab: tiles picked on the sheet; Shapes (F for full, presets for
  slopes and halves applied to every selected tile, or drawn with Tab), one way, surface,
  weight, data; Terrains (modes, layout templates over a box, marks painted, missing
  combinations with where they are needed); Animation by selecting frames.
- **Painting:** Tab over the viewport on a tile map paints (as it edits faces and joints):
  left paints, right erases, Shift a line, Ctrl+Shift a rectangle, Ctrl+click picks (a
  drag picks a stamp), B fills (Shift+B every same tile), Z and X turn, C and V flip,
  PageUp and PageDown change the layer; the Tiles panel at the bottom (Alt+T) with the tile
  set, patterns and terrains; random by weights; one undo step a stroke.
- **Physics shapes:** colliders edited in the view (Tab on a polygon), Trace from Pixels on
  a sprite (alpha threshold, simplification, a preview), a layer's merged outline seen with
  Alt+C.
- **Lights:** ranges and occluders drawn as the sound gizmos are; occluders traced from
  pixels or given per tile.

### Scripts (question 10: a)

The public scripting API is the team's (CLAUDE.md §3). Proposed: the components' fields as
any component; `tilemap:get(x, y)`, `tilemap:set(x, y, tile)`, `tilemap:cell(position)`,
`tilemap:terrain(x, y)`, `tilemap:set_terrain(x, y, name)` and `tilemap:data(x, y, name)`;
2D queries as the 3D ones (`physics2d.raycast`, `physics2d.overlap`, `physics2d.cast`);
`on_touch` and `on_untouch` from sensors.

## Measurements and arguments (estimates; measured in each step)

From ADR 0015's ref-1 numbers (1080p, 8-pixel sprites, an 8-bit target): the RTX draws a
covered pixel of a sprite in about 0.026 ns (1,000,000 sprites in 1.64 ms), the Radeon in
0.36–0.57 ns (1,000,000 in 23 ms, 10,000 in 0.365 ms); recording costs 7 ns a quad,
y-sorting 5.5, copying 1–2, preparing and uploading 100,000 quads 1.14 ms (RTX) and 0.76 ms
(Radeon). Tiny sprites carry more vertex work per pixel than a full-screen quad, so these
bound the cost from above; the Radeon's upper figure stands in for the scene colour's
16-byte blended pixels.

| Case | RTX 4060 Ti | Radeon 2-CU |
|---|---:|---:|
| A full-screen flat tile layer at 1080p (2.07 Mpx; the lookup's extra read counted at +30 %) | about 0.07–0.1 ms | about 1.0–1.5 ms |
| A 256 × 256 map, 16-px tiles at 3× (920 cells in view, at most 6 chunks a layer): two layers and 900 sprites of 48 px at 1080p | about 0.2–0.25 ms | about 3–4 ms |
| The same map at a pixel-art 640 × 360, scaled 3× | about 0.02–0.03 ms | about 0.3–0.45 ms |
| The map zoomed out whole (1.17 Mpx a layer), one quad a chunk | about 0.04 ms a layer | about 0.55–0.85 ms a layer |
| The same as a quad per tile (65,536 a layer) | about 0.1 ms a layer, plus about 1 ms of CPU | about 1.5–2.2 ms a layer, plus about 1 ms of CPU |
| A full screen of normal-mapped sprites lit by 8 shadowed lights that each cover it (the worst case), 1080p: shadow strips with five taps; light textures | about 0.2–0.7 ms; about 0.15–0.35 ms | about 4–10 ms; about 3–5 ms |
| The same with torch-sized lights (a twentieth of the screen each) | about 0.01–0.03 ms; about 0.05–0.1 ms | about 0.2–0.5 ms; about 1.1–1.6 ms |
| Either, at 640 × 360 | about a ninth | about a ninth |

- **CPU per frame:** a flat layer costs a quad per visible chunk; sprites about 15–20 ns
  each to record, sort and upload (1,000: 20 µs; 10,000: 0.2 ms; 100,000: 2 ms, where ADR
  0015's next step records on several jobs), on top of reading their poses, measured in
  E13-1.
- **Memory:** 4 bytes a cell (256 KiB for a full 256 × 256 layer on each side); 32 bytes a
  tile in the tile set's table.
- **Text:** a fully painted 256 × 256 layer is 2,048 row lines, about 70 KB when rows are
  long runs and about 320 KB when every cell differs; Godot writes the same layer as one
  line of about 1 MB.
- **Edits:** a painted cell writes 4 bytes and uploads its 4 KiB chunk; a terrain edit looks
  up its 3 × 3 cells; a chunk's outline traces 1,024 cells, off the frame, and replaces a
  few hundred segments at the next tick.
- **The Radeon's output** (tone mapping drawn apart, 0.8 ms, and copied to the monitor's
  GPU, 0.6 ms, at 1080p; `docs/perf/history.md`, 2026-10-10) is paid by every game; a
  pixel-art game's 2D work is small beside it.

## Steps

| Step | What | Needs |
|---|---|---|
| **E13-1** | sprites in scenes: `Sprite`; images' sprite settings in `.meta`; the orthographic `Camera` with its reference size; `ToneCurve::None` and neutral 2D views; the player's 2D pass (recorded and interpolated, layers, y-sort, sort groups); the editor's 2D view (pan, whole-number zooms, grids, snapping, picking by pixels, the 2D gizmo, outlines); images dropped as sprites | questions 1, 2 |
| **E13-2** | slicing and flipbooks: the image's tab (grid, islands and hand slicing, names, pivots, nine-slice borders, re-slicing that keeps names), slices as parts; `Sprite.frame`; sprite clips in `.meta` and from Aseprite tags, played by the `Animator` with its graphs and events; Aseprite files imported; sliced and tiled modes | questions 6, 7 |
| **E13-3** | tile maps: `.tileset` and its tab (sources, shapes, animation, weights, data), `TileMap` with its cells in the scene; flat layers as a quad a chunk, sorted layers as quads; the Tiles panel and painting (brush, eraser, line, rectangle, fill, picker, random, stamps, patterns, turns and flips) | questions 3, 8, 10 |
| **E13-4** | auto-tiling: terrain layers, blob and corner modes, layout templates, missing combinations (4a); then decoration rules (4b) | questions 4, 5 |
| **E13-5** | 2D physics: Box2D 3.2; `ScenePhysics2d`, `Body2d`, `Collider2d`; the shim's polygons, chains, sensors and pre-solve; colliders edited in the view and traced from sprites; tile collision as chains per chunk; one-way tiles; sensors to scripts | questions 2, 10 |
| **E13-6** | 2D lights: `Light2d`, `Occluder2d`, occluders from tile sets, normal maps; lit sprites' own pipelines; the shadow technique measured | question 9 |
| **E13-7** | later: the pixel-perfect camera's sub-pixel smoothing (render one pixel larger and shift) and a sharp-bilinear filter for fractional scales, atlases where measured to pay, isometric and hexagonal grids (question 8), sprite shapes along splines, 2D particles (ADR 0081's pool in 2D views), 2D bones, LDtk and Tiled imports | |

Measured as they land: the 2D pass on both GPUs at 1080p and at 640 × 360 (sprites from
1,000 to 100,000, tile layers zoomed in and out, the lookup against a quad per tile); no
allocation with sprites moving, tiles painted and animated, terrains made and lights on; a
view without 2D, and a 2D view without lights, paying nothing (registers included); a 2D
view's colours exact through the neutral output; the outline's rebuild for 256 × 256 and
1,024 × 1,024 layers and the step's cost against a box per tile; the panels within ADR
0107's 1 ms of UI.

## Consequences

- New source files: `.tileset`, `.sorting`, and Aseprite files as assets; sections in
  images' `.meta`; new components in scenes, saves and the network schema (tile maps' cells
  not replicated in E13). Packs gain tile maps' cells and their made tiles.
- `engine_render2d` gains the chunk lookup's pipeline and, in E13-6, lit sprites' ones;
  `engine_render` a `ToneCurve::None`; `engine_physics2d` Box2D 3.2, its shapes and
  callbacks; `Camera` its orthographic projection; the frame renderer a 2D pass for
  orthographic views.
- Box2D's length unit is global to the process (`b2SetLengthUnitsPerMeter` must be set
  before any other call): question 2's answer is a project-wide choice.
- Tile collision is hollow: games and the 2D mover ask the grid whether a point is inside
  solid tiles.
- Scripts gain calls the team approves.

## Questions for the team

The two notes' questions, merged. An answer can be one line, such as "1a, 2a, 3a, 4a, 5c,
6a, 7a, 8a, 9a, 10a".

1. **How 2D objects live in the engine.** Every object today is an entity with a 3D
   `Transform`, and everything is built on it: hierarchy, smooth motion between ticks,
   picking, the gizmo, undo, prefabs, saves, network play, scripts and animation tracks.
   - a. **One world** (Unity and Bevy): a sprite is an ordinary entity lying in the XY
     plane, turned only about z. Every tool works on sprites from the first day, and 2D
     and 3D can share a scene (a 3D level with a 2D overlay). Cost: 48 bytes a transform
     where 2D needs 20 (4.8 MB against 2 MB for 100,000 sprites) and 3D math where 2D
     would do (a fraction of a millisecond for 10,000 moving sprites, measured in E13-1);
     the 2D tools keep z and the other two turns at zero.
   - b. **A 2D transform and 2D-only scenes** (Godot's `Node2D`): smaller and a little
     faster per object. Cost: a second hierarchy, interpolation, gizmo, inspector,
     picking, script type, network schema and track kind to build and keep in step; 2D
     inside a 3D scene only through a viewport's texture.
   - Recommendation: a. Under either, 3D models in an orthographic view wait for an ADR of
     their own.
2. **The unit of distance in 2D.** The engine measures in metres (3D physics, sound ranges,
   navigation), and Box2D is tuned for objects of 0.1 to 10 m. Pixel art thinks in pixels.
   Box2D's unit is one setting for the whole program, so this is decided once per project.
   - a. **Metres:** each image says how many of its pixels make a metre (16 by default: a
     16-pixel tile is 1 m; 100 for high-resolution art); the 2D view and the inspector
     show pixels beside metres; gravity is 9.81. Cost: a second number shown in the
     inspector; nothing at run time.
   - b. **Pixels** (Godot, GameMaker): positions, sizes and gravity in pixels (Godot's
     default gravity is 980 pixels a second squared), Box2D told a character's height in
     pixels. Cost: sound ranges,
     navigation radii and every default tuned in metres must be retyped in pixels; a scene
     mixing 2D and 3D cannot agree; new art at another resolution changes every number in
     the game.
   - Recommendation: a, 16 pixels a metre by default.
3. **Where a tile map's cells are kept.** One layer of 256 × 256 cells is 65,536 tiles, and
   scenes are text that must merge in git. Godot keeps a layer as one binary line (a user's
   was 590,000 characters that no merge tool could handle); Unity keeps index tables that a
   merge silently broke ("won't fix").
   - a. **In the scene file, by rows of 32-cell chunks,** through a palette of short keys
     made from the tiles themselves, with runs (`a3kq*12`). Two people painting apart
     change different lines and git merges them; a prefab instance can override single
     rows; replacing every grass tile is one line. Cost: 70–320 KB of text for a fully
     painted 256 × 256 layer, read in a few milliseconds (measured in E13-3); the cook
     writes the binary form into packs.
   - b. **In a file of their own beside the scene** (`.tilemap`, the same text): the scene
     stays short and a map can be shared by scenes. Cost: one more file kind to keep with
     the scene; no per-row overrides in prefab instances.
   - c. **Binary in the scene** (Godot): smallest and fastest to load. Cost: no merges; a
     conflict loses one person's painting.
   - Recommendation: a.
4. **What a terrain layer keeps.** With auto-tiling you paint "grass" and "dirt" and the
   engine picks the edge and corner tiles. Either the picked tiles are kept, or what you
   painted is kept and the tiles are picked again when the map loads.
   - a. **What you painted** (LDtk, Unity's Rule Tiles): the tiles are made at load and
     around each edit, the same on every machine; the cook keeps the made tiles in packs,
     so a shipped game runs no terrain code. Fixing an edge tile in the tile set fixes
     every map at once; a tile placed by hand in a cell stays there. Cost: a lookup per
     cell at load in the editor and in unpacked runs (well under a millisecond for
     65,536 cells, measured in E13-4).
   - b. **The picked tiles** (Godot, Tiled): the terrain brush is only a painting tool.
     Cost: a change to the tile set's terrains changes nothing already painted; a painted
     cell rewrites up to nine cells, so edits touch and conflict on more lines; reading the
     terrains back from tiles needs a solver like Godot's, slow and order-dependent.
   - c. **Both** (LDtk's file): Cost: twice the data, and two copies that disagree after a
     merge until made again.
   - Recommendation: a.
5. **How tiles are picked from terrains.** Tile sheets come in standard layouts: 47 tiles
   for a wall against anything (the "blob"), 16 for two grounds meeting at corners, fewer
   when tiles may be turned or mirrored. Engines either mark each tile's sides and corners
   (Godot, Tiled) or use rules (LDtk, Unity's Rule Tiles).
   - a. **Terrains only,** in two modes: blob (a cell's tile chosen by which of its eight
     neighbours share its terrain) and corners (tiles drawn half a cell off the painted
     grid, each chosen by the four cells at its corners: 16 tiles for two terrains, 6 with
     turns). A template marks a whole standard sheet in one command; missing combinations
     are listed. Cost: decoration (flowers on a tenth of the grass, a shadow under every
     wall top) is painted by hand.
   - b. **Rules only** (LDtk): patterns of neighbour conditions with chance, flips and
     ordering. Cost: more setup for the common walls and grounds (LDtk needs its assistant
     for them); the rule grid is abstract to learn.
   - c. **Both, terrains first:** a in E13-4a, LDtk-style decoration rules over the same
     painted terrains in E13-4b. Cost: both editors, one after the other.
   - Recommendation: c.
6. **How a sprite plays its frames** (a run of frames 8 to 13 at 80 ms each). The engine
   already has an `Animator` (E12) with state machines, cross-fades, events such as
   footsteps, saves and network play.
   - a. **The `Animator` plays sprite clips:** a clip is a frame range with durations, from
     the image's settings or an Aseprite tag; idle → run → jump is an `.agraph`; events sit
     on frames; scripts call `animator:play("run")` as for 3D characters. Cost: the
     `Animator` learns a frame track (frame numbers that jump instead of blending, and a
     cut where 3D characters cross-fade).
   - b. **A component of its own** (Godot's `AnimatedSprite2D`): play and stop by name.
     Cost: a second animation system without graphs, events or saves; Godot has both, and
     users must choose between them.
   - Recommendation: a.
7. **Where sprite settings live:** pixels per unit, pivot, filter, slices (named
   rectangles), nine-slice borders and clips of an image.
   - a. **In the image's `.meta`** (Unity), the file beside each image that already holds
     its id and import settings; slices are parts of the image (`hero.png#run_3`), as a
     model's meshes are; an Aseprite file keeps its own in itself. Cost: a big sheet's
     `.meta` grows by a line or two a slice.
   - b. **A file per sprite and per flipbook** (Paper2D; Godot's `SpriteFrames`). Cost:
     hundreds of files for one character; ids to keep when a sheet is cut again; the asset
     browser full of them.
   - Recommendation: a.
8. **Which grid shapes E13 builds.** Squares serve platformers and top-down games;
   isometric diamonds and hexagons serve strategy games. Each shape touches drawing,
   painting (the cell under the pointer), terrains (six neighbours for hexagons),
   collision outlines and sorting.
   - a. **Squares only in E13,** isometric and hexagonal later (E13-7); the file has its
     `grid` field from the start, so maps made now stay valid. Cost: isometric and
     hexagonal games wait.
   - b. **Squares and isometric diamonds** in E13-3. Cost: about a third more grid work in
     E13-3, and diamond terrains and collision in E13-4 and E13-5 (estimates).
   - c. **All three.** Cost: about twice E13-3's grid work, and hexagonal terrains need
     layouts of their own (estimates).
   - Recommendation: a.
9. **What 2D lighting E13 builds.** Lit 2D (torches in a dungeon, shadows behind crates)
   needs lights, occluders and lit sprites; an unlit game pays nothing in any case. The
   Radeon iGPU is the budget.
   - a. **Lights with shadows and normal maps:** point, spot, directional and ambient
     lights; occluders from polygons and tile sets casting hard or filtered shadows;
     sprites with normal maps; the shadow technique picked by measuring Godot's and
     Unity's on both GPUs. Cost: E13-6 as planned; in the worst case (a full 1080p screen
     of lit sprites, 8 shadowed lights each covering all of it) about 3–10 ms on the
     Radeon depending on the technique (estimated; measured in E13-6), about a ninth of
     that at 640 × 360, and under half a millisecond with torch-sized lights.
   - b. **Lights and normal maps without shadows** in E13. Cost: sooner and cheaper;
     no shadows until a later step.
   - c. **a, plus soft shadows and bounced light** from a distance field (radiance
     cascades, as in Path of Exile 2). Cost: several full-screen passes a frame, estimated
     beyond the Radeon's budget at 1080p; its own research and patent check.
   - Recommendation: a; c later, as its own step.
10. **The scripts' calls for 2D.** Components' fields (a sprite's frame, colour and flips,
    a body's velocity) reach scripts with no new calls (ADR 0114). Tile maps and 2D physics
    need calls of their own, which stay supported once games use them.
    - a. **The proposed set:** `tilemap:get(x, y)`, `tilemap:set(x, y, tile)`,
      `tilemap:cell(position)` (the cell under a point), `tilemap:terrain(x, y)`,
      `tilemap:set_terrain(x, y, name)`, `tilemap:data(x, y, name)` (a tile's own value,
      such as its footstep sound); `physics2d.raycast`, `physics2d.overlap` and
      `physics2d.cast` as in 3D; sensors' `on_touch` and `on_untouch`. Cost: E13-3's and
      E13-5's calls, and the type definitions for them.
    - b. **Reading only in E13:** get, cell, terrain, data and the queries; tiles changed
      from Rust only. Cost: no destructible tiles or level changes from Luau until later.
    - c. **Fields only.** Cost: anything with tiles needs Rust.
    - Recommendation: a.

## As built (E13-1a, 2026-10-30): sprites in scenes and the player's 2D pass

The runtime half of E13-1; the editor's 2D view (pan, whole-number zooms, grids, picking,
the 2D gizmo, images dropped as sprites) is E13-1b.

- **`engine_app::Sprite`** (`sprites.rs`), plain data (`bytemuck::Pod`) so a game's
  `NetSchema::replicate::<Sprite>()` takes it; reflected, so scenes, saves and scripts take
  it as any component:
  - `image: AssetRef`, `frame: u32` (E13-2's slices and clips; a slice draws its whole
    image until then), `color: [f32; 4]` (linear, straight alpha, HDR allowed; the
    inspector's color hint), `flip_x`, `flip_y`, `size: Vec2` (a 0 takes the image's
    pixels over its pixels a metre), `mode` (`SIMPLE`, `SLICED`, `TILED`: E13-2's, drawn
    simple until then), `layer: i16`, `order: i16`, `hidden`. On-off fields are `u32` (0
    off), as `Camera`'s are: a `bool` would make it not plain data.
  - `AssetId` and `AssetRef` are `bytemuck::Pod` (`repr(C)` of `u64`s): plain-data
    components may name assets.
- **`SortGroup { layer, order }`**: its subtree sorts as one sprite at its place among the
  rest (its layer; its own entity's y on a layer sorting by y, else its `order`); its
  sprites are drawn together in the order of their own `order` (then the order they were
  recorded in), so two characters standing at the same y never interleave. A group inside
  a group sorts on its own (Unity's nesting is left for later).
- **`SortingFile`** (`.sorting`, version 1, the editor's grammar): `layers[<number>].name`
  and `layers[<number>].y_sort`, keyed by the layer's number (a sprite's `layer`), so
  adding one adds lines. `PlayerSettings::sorting` names it (`main.sorting` under the
  project, as `AudioSettings::layout` names the mixer); without it every layer sorts by
  order; `Player::asset_changed` reads it again and `Player::set_sorting` takes the
  editor's. Not yet an `AssetKind`: the editor's browser matches every kind, and E13-1b
  adds it with the editor's side.
- **An image's `.meta`** gains `sprite.pixels_per_unit` (16), `sprite.pivot` (`[0.5,
  0.5]`, shares of its width and height from its bottom-left corner) and `sprite.filter`
  (`Nearest`, or `Linear` for painted and high-resolution art), written only when not the
  defaults; slices, borders and clips come as more lines of the section (E13-2).
- **The orthographic `Camera`** (`engine_runtime`): `projection` (`PERSPECTIVE` 0,
  `ORTHOGRAPHIC` 1), `height` (metres from the image's bottom edge to its top),
  `reference` (`[0, 0]`: the output's size) and `post_effects`;
  `Camera::orthographic(height)` (its planes a kilometre before and behind it, no
  antialiasing) and `Camera::pixel_art(reference, pixels_per_unit)` (the height that makes
  a pixel of art a pixel of the reference). `ViewCamera::orthographic` and
  `ViewCamera::camera_2d` in `engine_frame`; UI placed on the camera follows its
  projection.
- **Neutral 2D views:** `ToneCurve::None` is the tone mapper's pass-through (no entry point
  gains code; the output clips: an 8-bit image at white, an HDR display at its peak).
  `CameraView` gives an orthographic camera `ToneCurve::None`, no bloom and no grading
  unless `post_effects` is set (then the player's own), its exposure (1 for the
  constructors) and, with a reference, `render_size` and `Upscale::Integer`. Color vision
  correction stays the player's: with no curve the grading LUT is built by `gradeLutNone`,
  an entry point of its own made the first time such a view grades.
- **`FrameRenderer::prepare_2d` and `add_view_2d`:** a 2D view's canvas drawn into the
  scene's HDR color at the render size over the view's clear color, in place of the scene
  passes; then exposure, the upscale (integer), bloom and tone mapping as any view's. An
  empty canvas declares no 2D pass, only the clear. Its depth is cleared to the far plane
  for UI placed in the world, a pass the graph culls when nothing reads it. A 2D view keeps
  no temporal antialiasing (a temporal upscale falls back to bilinear), no MSAA, no frame
  generation or motion vectors, no shadow cache, occlusion state, reflections, depth of
  field or motion blur: nothing of theirs is made for it. The 2D renderer for the HDR
  format is made with the first 2D view (`Renderer2d` with its existing shaders).
- **The player:** when the main camera is orthographic, each frame shows the sprite images
  made by now (`Assets::show_sprite_images`, after the texture pool's frame start) and
  records the canvas (`Sprites::record`), then `add_view_2d`; `FrameReport::sprites` is its
  CPU time, `Player::sprites_recorded` what it drew and culled. A perspective main camera
  records nothing and declares no 2D pass.
  - **Between ticks:** `Runtime::frame_with` (and `frame_driven_with`) call the player's
    extraction after each tick's, the transforms propagated: the sprites and sort groups in
    chunks whose `GlobalTransform` changed keep their pose before and after the tick and
    the tick they moved in, as `RenderExtract` keeps its instances'; a frame draws each at
    its pose `alpha` of the way between them when it moved in the last tick, else where it
    is. A sprite is drawn once a tick has placed it (its world transform propagated), as an
    instance is. A joined client's frame draws them where its frame put them.
  - **Recording:** a table by entity index keeps the poses and each sprite's image slot
    (compared by `AssetRef`, so a frame looks nothing up by hash); each sprite's quad is
    its image's size over its pixels a metre (or its own), its pivot at its entity, flips as
    UVs, its image's sampler; culled by the view's rectangle (turned with the camera); keyed
    `Order { layer, key }` with key `-y` on a layer sorting by y, else `order`; a sort
    group's sprites gathered and drawn after the rest, each group together in its own order.
  - **Images:** `Assets::sprite_slot` reads the image's `.meta` on first use and gives the
    loader a `Job::Image` (the material images' path: decoded with its levels in the
    background, compressed where the editor's cache is unless the `.meta` says
    uncompressed); a file read again re-reads its settings and loads again (it is not drawn
    for the frames its new texture takes). The frame asks for new sprites' images before
    the models' sync, so the frame that waits for its assets waits for them too.
- **Measured** (ref-1, release, `engine_app` examples/sprite_bench.rs: n 16-pixel sprites
  moving every tick on a layer sorted by y, an offscreen 1920 × 1080 output, medians of 300
  frames; `docs/perf/history.md`). The 2D pass at 1080p: 1,000 / 10,000 / 100,000 sprites
  17 / 89 / 728 µs on the RTX, 336 / 966 / 7,395 µs on the Radeon; at a pixel-art 640 × 360
  shown at 3×: 12 / 71 / 623 µs and 98 / 717 / 7,114 µs. Recording them: 20–25 / 187–208 /
  2,019–2,597 µs of CPU (20–26 ns a sprite: the estimate's 15–20 with the poses read). A
  thousand moving sprites, some culled, some in sort groups, allocate nothing
  (`tests/zero_alloc_sprites.rs`, both GPUs).
- **Tests:** every 8-bit value of a painted image reaches an 8-bit sRGB output exactly
  through the neutral 2D view, the scene's half-float target and 2× integer scaling (1,024
  pixels on each GPU); layers, order, y and sort groups by pixel; half a metre a frame for
  a metre a tick at two frames a tick; a perspective view with sprites declares no 2D pass
  and records nothing, an empty 2D view the clear alone; sprites, sort groups and pixel-art
  cameras through scene files and saves; the golden shot `sprites_2d` (per vendor); the
  tone mapper's GPU test with `ToneCurve::None`, plain and graded.
- **Left (E13-1b and later):**
  - the editor's 2D view and tools; `.sorting` as an asset kind in the editor's browser;
  - slices, flipbooks, sliced and tiled modes (E13-2); tile maps (E13-3);
  - recording on several jobs (ADR 0015's next step) past ten thousand sprites;
  - a sprite image read again is not drawn until its new texture is made;
  - 3D models in an orthographic view, and sprites in a perspective one, wait for an ADR
    of their own;
  - the pixel-perfect camera's sub-pixel smoothing (E13-7).

## As built (E13-1b-1, 2026-11-01): the editor's 2D view

- **The view turns 2D** by **View: 2D** (the palette; again for 3D, the 3D camera kept as
  it was): the editor's camera looks down −Z onto XY, orthographic
  (`OrbitCamera::flat`, its height in world units; the camera entity's `height`, so the
  player draws the view with the 2D pass, 3D content not in it, as decided).
- **Navigation:** the right or middle button's drag pans (the scene follows the pointer at
  any zoom); the wheel zooms **about the pointer by whole numbers of the art's pixels**
  (100 %, 200 %, 300 % …, below 100 % halves; 16 art pixels a metre, a sprite's default:
  `OrbitCamera::zoom_2d`), so pixel art never shimmers in the editor; **Ctrl+0** is 100 %;
  F frames the selection in XY; bookmarks (Ctrl+1..9) keep 2D views as 3D ones.
- **Picking and the gizmo** work through the same view (rays from the orthographic
  projection; a pixel's world size the same at every depth).
- **Tests:** the zoom's steps from 330 % (400, 200, 100, 50, 25, back to 100 %), a drag's
  pan at any distance, no turning, framing in XY, the projection's top edge.
- **E13-1b-2:** a scene whose main camera (the highest priority active one) is
  orthographic opens in the 2D view, a perspective one's in 3D; **an image dropped on the
  2D view is a sprite** of it at the pointer on the XY plane, its place on whole art
  pixels, selected, one undo step (test: dropped, snapped, one step, 3D back as it was).
- **E13-1b-3:** in the 2D view positions step by the art's pixels (1/16 m: Shift moves by
  one, Ctrl lands on one, under the owner's rules), the 3D step back in 3D; **the pixel grid
  from 800 %**, a faint line on every art pixel's edge in sight (a reused vector: nothing
  allocated a frame once grown). Test: the step, no grid at 400 %, a line per 8 screen
  pixels at 800 %.
- **E13-1b-4:** **the game camera's frame** in the 2D view: the rectangle the scene's main
  orthographic camera shows, at its reference size's shape (16:9 without one), in amber.
  **A fix to E13-1b-1:** the editor's camera in 2D now sets `Camera::projection` to
  orthographic (it set only the height, which does not make a camera orthographic, so the
  2D view drew in perspective); a test checks it. **The gizmo is 2D as it is:** in the
  orthographic view the z arrow points at the eye and is left out (an axis seen end-on),
  the x and y arrows, the square between them and the ring about z remain.
- **Left (E13-1b-5 and later):** Ctrl snapping to a tile map's cells (with E13-3); the 2D
  gizmo's own look (x and y arrows, the square,
  the ring about z); picking sprites by their pixels and their outlines.
