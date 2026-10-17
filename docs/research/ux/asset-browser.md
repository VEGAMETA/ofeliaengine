# Asset browser

Checked: 2026-10-17. Products were studied as products (behaviour, documentation); Godot
(MIT) was read for ideas only. No code was carried over. How assets are identified is in
`docs/research/editor-architecture.md` §4. Input for [ADR 0107](../../adr/0107-editor.md)
and [ADR 0108](../../adr/0108-scene-format-2.md).

CLAUDE.md §12: asset browser with previews; nothing blocks the UI (import is a background
task); errors next to their cause; drag and drop where natural. CLAUDE.md §13: hot reload
for any asset.

## How four editors do it

| Editor | Layout | Import | Finding and references | What works | What annoys |
|---|---|---|---|---|---|
| Unity (Project window) | a folder tree beside a grid of thumbnails; a size slider | automatic on change; settings in the inspector; GUIDs in `.meta` files | search with type and label filters; favourites; "Find References In Scene" | import settings where the asset is selected; drag into the scene, the tree or a field | `.meta` files to keep in sync |
| Unreal (Content Browser) | sources, filters, collections, rendered thumbnails | import by hand or on change | Reference Viewer, Size Map, asset tooltips | references and sizes at a glance; collections | redirectors left after renames until fixed up |
| Godot 4 (FileSystem dock) | a tree, or a tree beside a list or thumbnails | automatic; an Import dock for settings and reimport; `.import` files | a filter; favourites; dependencies; "View Owners" | the uid survives moves; the layout the team knows | a separate Import dock; reimport by hand after changing settings |
| Blender (Asset Browser) | libraries and catalogues (virtual folders) with previews | assets are marked in .blend files | search, catalogues | dragging an object in places it on the surface under the cursor | only marked assets; libraries set up by hand |

## Scenarios

| The developer wants to… | Ours |
|---|---|
| find the rock mesh among 3,000 assets | type "rock" in the browser's search (`mesh:` narrows by type), or `#rock` in the palette |
| bring a new model in | copy it into the project folder in Explorer: it is imported in the background, a thumbnail appears, a problem shows as a badge on it |
| give a wall a different material | drag the material from the browser onto the wall: the slot under the cursor takes it, one undo step |
| rename an asset without breaking scenes | F2 in the browser: the file and its id file move together, references keep working |
| know where a texture is used | "Find references": the scenes, prefabs and materials that use it, each a click away |
| change a texture's compression | select it: its import settings are in the inspector; a change reimports it in the background |

## What we take

1. **Layout:**
   - under the viewport (`editor-layout.md`): a folder tree beside a grid or a list;
   - a thumbnail size slider; favourites and recent files.
2. **Import never blocks** (§12):
   - a changed or new file is imported in the background (the cook into the DDC);
   - the asset shows its state: importing, ready, or an error badge that opens the error
     with its cause;
   - running games and the editor swap the new version in (§13).
3. **Thumbnails are rendered by our renderer offscreen:** meshes, materials, scenes.
   Textures are shown as themselves, sounds as waveforms. They are cached in the local
   cache.
4. **Search:** the palette's matcher, with type filters (`mesh:`, `material:`,
   `sound:`, `scene:`).
5. **Import settings in the inspector** when an asset is selected (Unity, Godot). They are
   kept beside the source file ([ADR 0108](../../adr/0108-scene-format-2.md)). A change
   reimports, nothing to press.
6. **Renames and moves inside the editor** move the asset's id with it, and references
   never break. Files moved outside the editor are found again by id or content.
7. **References:** "Find references" (where an asset is used) and "Dependencies" (what it
   uses), each a click away (Unreal, Godot).
8. **Drag and drop:**
   - into the viewport: placed on the surface under the cursor (`viewport-tools.md`);
   - onto an object: a material goes to the slot under the cursor;
   - onto a tree row: the asset becomes its child;
   - onto an inspector field: assigned.
9. **"Show in Explorer"** and "Open in VS Code" for scripts.

## What we do not take

- Redirectors left behind by renames (Unreal): references are rewritten through ids.
- An import button or a separate import dock (Godot): settings live in the inspector,
  and import is automatic.
- An in-editor asset store (cut in `editor-layout.md`).
