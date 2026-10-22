# 0112. Asset previews: thumbnails and the inspector's previews

- Status: Accepted (2026-10-22): the design and its two steps; E4-6a built the same day
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
