# 0112. Asset previews: thumbnails and the inspector's previews

- Status: Accepted (2026-10-22): the design and its two steps, both built the same day
- Date: 2026-10-22
- Builds on: [ADR 0016](0016-viewports.md) (viewports into textures),
  [ADR 0072](0072-frame-renderer.md) (the frame renderer's views),
  [ADR 0107](0107-editor.md) (the editor; E4's browser and asset fields),
  [ADR 0108](0108-scene-format-2.md) (asset ids, the database, background loading)
- Research: [docs/research/asset-previews.md](../research/asset-previews.md)

## Context

ADR 0107's E4 (team decision, 2026-10-17):
- the file browser with rendered thumbnails;
- asset fields in the inspector as in Godot, with "a square preview the engine renders
  with the asset's own materials and shaders".

What there is:
- **The FileSystem dock** lists files with their kinds' icons (E4-2).
- **`engine_ui` draws rectangles and text only:** no image.
- **The frame renderer** composes a view from a `SceneInput` (its draws a plain list or
  the persistent GPU scene) into a `ViewTarget`. Its tone mapping and post filters are
  made for one output format, the swapchain's.
- **The player makes assets in the background** (ADR 0108's E4-4) and makes them again
  when their files change (E4-5).

Godot, Unreal and Unity all draw previews with their renderer, in a scene of their own
with their own camera and lights, later than asked, and cache them; textures are their
own picture made smaller (research note).

## Options

**Where previews are drawn**

1. **A view of the frame renderer with a draw list (chosen).** The asset's meshes with
   their own materials, a key light and an ambient term, drawn into a square image of
   its own:
   - no temporal history (a cut each time), multisampled;
   - one at most per frame, for the assets on screen.
2. **A GPU scene or a world per preview** (Unreal's `FPreviewScene`, Godot's scenario).
   It costs a second scene's structures, and a draw list is enough for one asset.
3. **A rasterizer of our own on the CPU.** It would not draw with the asset's own
   materials and shaders, which the team decision asks for.

**The image's format**

1. **The swapchain's.** The cache would need converting from whatever the output is (an
   HDR10 output encodes PQ), and an HDR session's thumbnails would differ from an SDR
   one's.
2. **Its own, RGBA8 sRGB (chosen).** The frame renderer makes a format's tone mapping and
   post filters the first time a view's target asks for it. The same image is shown and
   cached.

**The cache**

1. **In memory only** (Unity's): an editor's every start draws them again.
2. **PNG files in `.ofelia/thumbnails/` (chosen, as Godot's):**
   - a file per asset, named by its id;
   - beside it, the key it was made for: the database's size and modification time (and
     the content's hash where the database has one), the preview's size, and a version;
   - a file written anew (E4-5's `ScanReport::changed`) is drawn again.

**Textures**

- **Their own image, made smaller** (never larger, keeping its proportions): decoded and
  reduced on the loading thread (ADR 0108's E4-4), uploaded as one level. Not drawn by a
  view.

## Decision

Options 1, 2, 2 above, built in two steps.

- **E4-6a: images and the UI.**
  - `engine_ui` gets an image: a rectangle showing a texture by its bindless index. The
    editor's painter draws it as a textured quad.
  - The dock shows its files as tiles, each a thumbnail over its name; Ctrl+wheel over
    the dock sizes them.
  - Image files' thumbnails are made on the loading thread and cached. Other kinds show
    their icons until E4-6b.
- **E4-6b: rendered previews.**
  - `engine_frame`: a view's target says its format, and the tone mapping and post filters
    of a format are made the first time one asks for it.
  - `engine_app`: the player draws asked-for previews. It draws an asset's draw list,
    framed by its box (turned 22.5° about Y and X), into an image of its own. It draws
    one a frame, once the asset's parts are made, and reads it back for the cache.
  - The editor asks for models' and materials' previews (a material on the built-in
    sphere), for the dock and for the inspector's asset fields.

## Consequences

- The editor's frame pays for at most one small view a frame while previews are being
  made, and nothing once they are cached.
- The frame renderer can draw views into textures of their own format: what CLAUDE.md
  §6's viewports in the UI and the world need too.
- `.ofelia/thumbnails/` grows with the project (an RGBA8 PNG of 128² is a few KB to
  ~60 KB); it is a cache, rebuilt at any time.

## As built (E4-6a, 2026-10-22): images and the UI

- **`engine_ui`:** `Style::image` puts a picture (`UiImage`: a texture's bindless index
  and its region) over the node's fill, drawn as `DrawCmd::Image`. The editor's painter
  draws it as a sprite.
- **The dock:**
  - its files are tiles: a picture over the name, the picked one lit;
  - a row holds as many as its width allows (`shell::tile_columns`);
  - Ctrl+wheel over the tiles sizes their pictures from 32 to 128 px (64 by default), the
    wheel taken from the panels and the viewport.
- **The thumbnails** (`engine_editor::thumbnails`):
  - made on a thread of their own: the image read and fitted into 128², never enlarged,
    centred on transparency, averaged in sRGB;
  - cached as `.ofelia/thumbnails/<id>.png` with `<id>.key` (the file's size and time, the
    side, a version);
  - asked for only for the tiles on screen, by the listing's rectangle and scroll of the
    last frame.
- **The atlas:**
  - the overlay's, 1024² RGBA8 sRGB, 64 cells, its zeros uploaded with the first frame;
  - up to 8 thumbnails are uploaded a frame, before the panels draw;
  - a thumbnail is shown once its pixels are in;
  - a new one takes a free cell or the one seen longest ago.
- **Checked:**
  - fitting keeps proportions and centres, and never enlarges;
  - averaging;
  - the cache is used for the same key and not for another;
  - in the editor, an image's tile shows its picture, a script's its icon, the cache is
    written, and Ctrl+wheel grows the tiles;
  - on the GPU, the dock captured with an image whose colours tell its orientation shows
    it upright.

## As built (E4-6b, 2026-10-22): previews the engine draws

- **`engine_frame`:**
  - `ViewSettings::target_format` names an image of the view's own format. Its tone
    mapper (`Tonemap::keeping_alpha`, a new entry point `fragmentAlpha`) is made the first
    time a view asks;
  - the scene's alpha is kept and the edges' color is taken out of their coverage, so the
    image is transparent where nothing was drawn;
  - such a view refuses a filter after tone mapping and resampling
    (`FrameError::TargetFormat`), and leaves the texture LOD bias, which the samplers
    share, to the frame's main view;
  - every other entry point's SPIR-V is unchanged.
- **Not multisampled:** the MSAA resolve writes an alpha of 1. A preview is drawn at
  twice its side with one sample, then averaged down 2 × 2, each pixel's color weighted by
  its samples' alpha. That gives four samples a pixel for edges and shading alike.
- **`engine_app`:**
  - `Player::request_preview(PreviewRequest { key, parts })` asks, and
    `take_previews` hands out `Preview { key, pixels }` (128², RGBA8 sRGB, empty when a
    part cannot be made);
  - the player draws one a frame, once each part's mesh is made and its material has its
    textures. It uses a view of its own: a key light, an ambient term, no sky, nothing of
    the scene's effects;
  - **framing:** the parts' box seen from the front, turned 22.5° about Y and looking
    22.5° down, as near as its corners allow inside a 20° lens;
  - **read back:** through one more buffer than the frames in flight.
- **The editor:**
  - the thread looks for a model's or a material's cache, and on a miss the player is asked;
  - a model's parts come from its file as a drop places them (E4-3's plan, the root at
    the origin);
  - a material goes on the built-in sphere;
  - the pixels are cached by the thread.
- **Checked:**
  - a view into its own format draws the same light, stored sRGB-encoded, keeps its alpha,
    and refuses FXAA;
  - the framing keeps every corner inside the lens;
  - the halving averages by alpha;
  - a model's and a material's previews are drawn, transparent around them (a tall post
    and a blue sphere), and a missing part is answered empty;
  - in the editor, a model's, an image's and a material's tiles all get their thumbnails,
    cached.
- **Left:** previews of scenes.

## As built (E4-6c, 2026-10-22): the inspector's asset fields

ADR 0107's E4 asks for asset fields as in Godot (team decision, 2026-10-17).
- **The field** (`Widget::Asset`) shows a 40 px preview (its kind's icon until there is
  one) and the asset's name, in a taller row. The inspector keeps each asset field's value
  (`Property::asset`).
- **What the preview is of** (`thumbnails::Subject`):
  - a model's `mesh` is that mesh alone, with the material its file gives it (the default
    one for a built-in mesh);
  - a model's `material` is that material on the built-in sphere (a glTF file's material,
    a material file, or the built-in one);
  - any other field shows its file's.

  Thumbnails are keyed by subject, each cached under its own name in
  `.ofelia/thumbnails/`.
- **Picking:** a click opens the palette on the project's assets of the field's kind
  (`#model:` for a mesh, `#material:` for a material, `#` for others). Enter gives the
  field the one picked, through the same path as a file dropped on it (every selected
  entity, one undo step, `Model`'s fields checked), not the dock. The palette opened the
  usual way goes back to the dock.
- **Checked:**
  - picking a material file for a box's model through the palette is one step, and the
    dock's pick does not move;
  - on the GPU, the inspector shows the crate's mesh and its grid material on a sphere.
