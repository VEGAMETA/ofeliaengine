# Asset previews: thumbnails and the inspector's previews

For ADR 0112 (E4-6). The asset browser shows rendered thumbnails
(`docs/research/ux/asset-browser.md` §3). The inspector's asset fields show the same
square preview (ADR 0107's E4, team decision 2026-10-17): "a square preview the engine
renders with the asset's own materials and shaders". How three engines make them, checked
2026-10-22.

## Godot 4 (MIT; read as code, master)

`editor/inspector/editor_resource_preview.cpp`, `editor/inspector/editor_preview_plugins.cpp`:

- **A queue and a thread.**
  - `queue_resource_preview(path, receiver)` answers from an in-memory cache at once, or
    queues the path.
  - A thread (when the renderer can create resources off the main thread) or the idle
    callback, up to 100 ms a frame, takes the queue in order.
  - One generator per resource type makes the preview (`EditorResourcePreviewGenerator`).
- **Meshes and materials** are drawn by the renderer itself, in a scenario and a viewport
  of the generator's own (`scenario_create`, `viewport_create`):
  - a 128 × 128 viewport, transparent background, updated only when asked;
  - two directional lights, a key from (−1, −1, −1) and a grey fill (0.7) from above;
  - a mesh is seen through an orthographic camera, its box centred and turned 22.5° about
    Y and X, scaled to fill the view;
  - a material is put on a UV sphere (32 × 32) seen through a 45° perspective camera;
  - the image is read back (`texture_2d_get`), made RGBA8, scaled to the thumbnail's size,
    its corners rounded.
- **Textures** are their own image, made smaller (never larger), cubic.
- **The disk cache** is in the editor's cache folder, per file:
  - `resthumb-<md5 of the path>.png`, a `_small.png`, and a `.txt` with the size, the
    file's modification time (the newer of it and its `.import`), its md5, the
    generator's metadata and a version;
  - a different time compares the md5: the same content keeps the thumbnails, else they
    are made again;
  - `check_for_invalidation(path)` drops a preview whose file's time changed.
- **Sizes:** the thumbnail size setting, and a small one the size of the editor's icons.

## Unreal (product and API documentation)

- **A renderer per class:** `UThumbnailRenderer` (`UStaticMeshThumbnailRenderer`,
  material, skeletal mesh, blueprint).
- **A scene of its own:** each holds a `FThumbnailPreviewScene` (an `FPreviewScene`: a
  world of its own) with one actor that shows the asset (`FStaticMeshThumbnailScene`'s
  `SetStaticMesh` before the next `CreateView`).
- **How often:** `GetThumbnailRenderFrequency` lets a renderer say how often it is drawn
  again.
- Where thumbnails are stored was not checked.

## Unity (documentation)

- **Asynchronous:** `AssetPreview.GetAssetPreview(asset)` is answered later:
  `IsLoadingAssetPreview` while it is being made.
- **Cache:** previews are kept in memory, its size set with
  `SetPreviewTextureCacheSize`.
- **Custom previews:** `PreviewRenderUtility` (little documented) gives a preview scene
  of its own with a camera and lights; editors draw their own previews with it.

Sources:
- [Godot, editor_resource_preview.cpp](https://github.com/godotengine/godot/blob/master/editor/inspector/editor_resource_preview.cpp)
- [Godot, editor_preview_plugins.cpp](https://github.com/godotengine/godot/blob/master/editor/inspector/editor_preview_plugins.cpp)
- [Unreal, UThumbnailRenderer](https://dev.epicgames.com/documentation/unreal-engine/API/Editor/UnrealEd/UThumbnailRenderer)
- [Unreal, FStaticMeshThumbnailScene](https://dev.epicgames.com/documentation/unreal-engine/API/Editor/UnrealEd/FStaticMeshThumbnailScene)
- [Unreal, FThumbnailPreviewScene (4.26)](https://docs.unrealengine.com/4.26/API/Editor/UnrealEd/FThumbnailPreviewScene/index.html)
- [Unity, AssetPreview](https://docs.unity3d.com/ScriptReference/AssetPreview.html)
- [chrisyarbrough/preview-scene, a custom preview scene in Unity](https://github.com/chrisyarbrough/preview-scene)

## What every one of them does

- Previews are drawn by the engine's own renderer, in **a scene of their own** with
  **a camera and lights of their own**: the asset is seen alone, the same way each time.
- They are made **later**, off the frame that asks, and **cached**: in memory, and on disk
  keyed by the file (its time, then its content).
- Textures are their own picture, made smaller.

## What we take

- **A scene of their own, as a draw list.** Our frame renderer already composes a view
  from a `SceneInput` whose draws can be a plain list (`SceneDraws::List`) into any
  `ViewTarget`. A preview is a view of its own:
  - its own state (`View`), with no temporal history (every frame a cut);
  - the asset's meshes with their own materials;
  - a key light and an ambient term;
  - drawn into a square image of its own.

  No second GPU scene or world is needed for one asset.
- **Godot's framing:**
  - a model's box centred and turned (22.5° about Y and X), filling the square;
  - a material on the built-in sphere;
  - a texture as itself, made smaller.
- **One preview a frame at most**, for the assets on screen, so the editor's frame stays
  in its budget; the rest wait their turn.
- **The disk cache in `.ofelia/thumbnails/`**, keyed by the database's entry (the id, the
  size and time; the content's hash where the database has it): a file written anew
  (E4-5's `ScanReport::changed`) gets a new preview.
- **An image command in the UI** (`engine_ui` draws rectangles and text only today): a
  rectangle with a bindless texture, drawn by the editor's painter as a textured quad.

## What we do not take

- A world or GPU scene per preview (Unreal's `FPreviewScene`, Godot's scenario): a draw
  list is enough for one asset, and costs no extra scene structures.
- A thread that draws: the GPU is driven from the frame. Decoding images for texture
  thumbnails runs on the loading thread (ADR 0108's E4-4).
- Rounded corners in the image itself: the UI rounds the tile.

No code was taken from any of them.
