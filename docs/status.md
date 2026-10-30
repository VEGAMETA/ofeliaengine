# Engine status and hand-off

Where the engine stands and how work on it is run. A new session (coordinator or agent)
reads this after `CLAUDE.md` and `docs/agent-rules.md`. Keep it current: whoever merges a
PR that changes the picture updates this file in the same PR.

Last updated: 2026-10-28.

## 0. Now (2026-10-22)

- **In progress: the editor** (queue item 9). Designed with the owner on 2026-10-17
  (#250: ADR 0107, ADR 0108; research `docs/research/editor-architecture.md`, UX notes in
  `docs/research/ux/`). The owner's choices:
  - the whole §12 list, not a first wave only;
  - the editor linked into the game's build behind an `editor` feature; a separate process
    later; the 2.5 s rebuild accepted for now, hot patching later;
  - prefabs in the first version;
  - tools in the order materials, sound, animation, 2D, then the rest; "главное чтобы для
    разработчика было удобно";
  - a built-in code editor at Godot's or Roblox Studio's level, VS Code beside it;
  - asset ids: ids inside our own files, a `.meta` beside foreign ones, references by id
    and path, the local database only a cache;
  - any scene can be a prefab.
  - **Where it stands (2026-10-28):** E1 to E9 are done (E2b but for the cook's path, E3
    but for the tree's marks on what changed, E8 but for rename and references across
    the project), and material slots on `Model`.
    - E7, scripting v1 (ADR 0114): reading scripts, scenes and prefabs from a release's
      packs comes with the one-button build; nothing the engine gives scripts allocates
      in steady state (ADR 0114's examples in the zero-allocation test).
    - E9, the script debugger (ADR 0119): logpoints, pause points and breakpoints from the
      gutter, steps, the Debugger panel, stops at errors, values at a stop, VS Code over
      DAP to the editor and to a development build started with `--debug`.
    - E10-1, materials in the inspector (one `.material` file, ADR 0120), E10-2, Slang in
      the code editor (`slangd`, Material: New Type), and E10-3, the material graph (ADR
      0123: canvas, search, values, preview, errors, reroutes, frames, `[Node]` and
      `.mfunc` functions), are done; E10-4 is under way: variants that follow their base
      (E10-4a, E10-4b), costs (E10-4c, E10-4f: registers, code, Draw as Fallback), the
      inspector's preview (E10-4d) and a material shown on an object while dragged over it
      (E10-4e) are in; per-node previews ahead.
    - E11, sound, is done: the mixer file and panel (meters, effects, sends, the room's
      strip, snapshots, ADRs 0121 and 0122), previews and fields, the viewport's gizmos
      and hearing in place, the falloff curve.

    The entries below are in the order merged.
  - **E1-1, scene format 2** (#251, ADR 0108 as built):
    - the saves' grammar is shared (`engine_scene::text`), and scenes gain strings,
      lists, maps and optional values;
    - `Name`, and structs of one field written as that field;
    - `AssetRef` written by path and id, and the scene's id in the header;
    - format 1 still loads;
    - saving 11 % faster, loading unchanged.
  - **E1-2, scenes in the player** (#252, ADR 0108 as built):
    - `PlayerSettings::scene` and `project`, `Player::load_scene`;
    - every reflected component of the program loads without registration;
    - `Model` names a mesh and a material by asset, and the player draws it;
    - built-in cube, sphere, plane and cylinder, the default material;
    - `.material` files.
  - **The owner added blockout** (CSG brushes as in Godot) as phase E2b, after the
    viewport tools.
  - **E1-3, the editor on the player** (#253, ADR 0107 as built):
    - the player draws the scene into the viewport panel (`set_view_rect`), and the
      editor paints its shell over the frame (`Overlay`);
    - the tree and the inspector read the real world;
    - the editor's camera;
    - the binary opens a scene file.
    - HDR output, lost here for a while, came back with the player's own (below).
  - **E1-4a, the editor edits the world** (#254, ADR 0107 as built):
    - undo records every component scenes save (strings and lists as scene lines);
    - the inspector drags numbers, vectors and rotations (in degrees), clicks
      checkboxes, picks enums; Escape takes a drag back;
    - snapping as the owner asked: Shift inverts the default (free), Ctrl rounds to the
      grid; position 0.1, scale 0.1, rotation 45°;
    - commands with shortcuts: undo, redo, save, delete, duplicate, add;
    - the scene saved under its own id; `*` on the tab while unsaved.
  - **HDR output in the player** (#255, ADR 0038 "In the player"): scRGB by
    default while Windows runs the display in HDR, HDR10 or SDR by setting; follows the
    display at run time; the editor's panels at the SDR white level. Tested offscreen
    (`assume_display`); not yet seen on an HDR display (ref-1's runs in SDR).
  - **E1-4b-1, text fields** (#256, ADR 0107 as built): rename in place (F2, a
    double click), numbers typed with expressions (`2*pi`, `+=0.5`, `*1.2`), strings, the
    tree's and the inspector's filters; Windows' clipboard.
  - **E1-4b-2, the command palette** (#257, ADR 0107 as built): Ctrl+Shift+P finds and
    runs every command (the add menu's kinds among them), `@` (Ctrl+P) finds the scene's
    objects; ↺ and Backspace reset a field to its default; Ctrl+C and Ctrl+V over a field
    copy and paste its value.
  - **E1-4b-3a, the selection** (#261, ADR 0107 as built): Ctrl+click, Shift+click and
    the arrows in the tree, Delete and Ctrl+D on the whole selection as one step, numbered
    names for new entities and copies, icons by kind in the add menu, the tree and the
    palette.
  - **E1-4b-3b, multi-editing** (#262, ADR 0107 as built): the selection's shared
    components, "—" where values differ, typed values to all and relative ones and drags
    from each one's own, one undo step.
  - **Scene format 3: the roots' order** (#263, ADR 0108; team decision 2026-10-20):
    `order = <n>` in a root's block, spaced so moving a root changes its line only; the
    editor's tree shows and keeps it; versions 1 and 2 load.
  - **E1-5a, the game's `main`** (#264, ADR 0107 as built):
    - `engine_app::main!(settings, start)` runs the player, or the editor with the game's
      `editor` feature;
    - `ofeliaengine new` writes the feature, and the quick start's scene and materials
      under fresh ids;
    - `ofeliaengine edit` runs the game in its editor;
    - the quick start is a scene, and `find_named` finds its cube and camera.
  - **E1-5b-1, autosave** (#266, ADR 0107 as built):
    - unsaved changes autosaved into `.ofelia/` after 30 s at a quiet moment, and brought
      back without a dialog after a crash or a clean exit;
    - a scene file changed on disk is never written over: the take-disk and save-over
      commands pick a side;
    - one editor per project.
  - **E1-5b-2, the view in the session** (#267, ADR 0107 as built): the camera, the
    selection, the tree's closed rows, the filters, the tool, snapping and recent commands
    come back at start; "Restore Unsaved Changes…" brings back an older autosave.
  - **E1-5c-1, rebuild and restart** (#268, ADR 0107 as built; team decisions
    2026-10-20, profile В and restart Б):
    - a code change builds the game in the background, its errors are shown by line, and
      the new build restarts the editor (by itself in the background, Ctrl+R in front)
      with the session back;
    - a change rebuilds in about 1.5–1.6 s and the restart takes 0.36–0.37 s;
    - `ofeliaengine new` writes the editor's dev profile, and `ofeliaengine edit` is the
      launcher.
  - **E1-5c-2, the restart without a gap** (#269, ADR 0107 as built): the new window
    opens hidden at the old one's place and shows its first frames over it before the old
    editor exits; a new build that fails at its start leaves the old editor running with
    the reason; any start opens the window where the session kept it. E1 is done.
  - **E2-1, the gizmo** (#270, ADR 0107 as built): move, rotate and scale handles at the
    selection's center, Q W E R for the tools, Space for local axes, the owner's
    snapping rules, one undo step per drag, Escape takes it back.
  - **E2-2, keyboard transforms** (#271, ADR 0107 as built; team decision 2026-10-20,
    Blender's keys by default, ours since E2-2b):
    - G, R, S; X, Y, Z for an axis (again: the local one) and Shift for a plane;
    - typed values and expressions; Enter or a click keeps, Escape or a right click
      takes it back;
    - a hint by the pointer, the axis drawn across the viewport;
    - Godot's keys through "Editor: Key Map…".
  - **E2-2b, our keys and the camera's flight** (#272, ADR 0107 as built; team decision
    2026-10-21):
    - Q, E, R transform at once, Shift+Q, E, R pick the gizmo's handles; Blender's and
      Godot's key maps in "Editor: Key Map…";
    - with the right button held, W, A, S, D fly the camera, Q and E down and up, Shift
      faster, and the wheel sets the speed (kept by the session).
  - **E2-3, picking and the outline** (#273 the design, ADR 0109, research
    `docs/research/viewport-picking.md`; team decisions 2026-10-21: hover, transparent
    pickable behind a switch, hidden parts of the outline dimmed, boxes by visible pixels
    with Alt for hidden ones, 2 px, Shift+click toggles in the viewport):
    - **E2-3a, a click picks** (#274): the view's culled commands drawn again into a
      32 × 32 square, slot ids read back; hover, click, Shift; 19–20 µs on the RTX,
      about 0.5 ms on the iGPU (Sponza);
    - **E2-3b, the outline** (#275): the selection (with its children; the active entity
      lighter) and the hovered entity, drawn alone into a mask bounded by their boxes on
      the screen, then a 2 px line, hidden parts at 35 %; three small instances cost 6 +
      10 µs on the RTX, 63 + 108 µs on the iGPU;
    - **E2-3c-1, what lies under the pointer** (#277): picks draw only the GPU scene's
      instances whose world box reaches the pixels (the square 7 µs instead of 18–20 on the
      RTX, 134 µs instead of 0.51 ms on the iGPU); a list of every instance under a pixel,
      front to back; a click again at the same spot cycles behind the active entity,
      Alt+click offers the list in the palette; "Pick Transparent Surfaces" kept by the
      session;
    - **E2-3c-2, boxes** (#278): a drag in the viewport draws a box that takes what shows
      in it (two passes over the candidates), with Alt hidden objects too; Shift adds,
      Ctrl takes out, Escape takes it back; what it takes is outlined while dragging. A
      whole-view box on Sponza: RTX 110 µs, iGPU 2.2 ms (one atomic per wave: from 1 ms
      and 3.4 ms on the RTX).
  - **E2-4, framing** (#280): F frames the selection (its world boxes from the GPU scene)
    or the scene, the camera flying there in 0.2 s; Shift+F follows the active entity;
    Ctrl+1..9 keeps a camera bookmark, 1..9 goes back, kept by the session. The gaps
    between docks are the window's color (#279: they showed the output's old memory).
  - **E2-5, surfaces** (#281): a surface pick (ADR 0109: the candidates under a pixel but
    some left out, ids and depths of a 3 × 3 square: point and normal); Ctrl+Shift drags
    the selection over surfaces, its box's face on them; End drops each selected entity
    onto the surface below it, as one undo step.
  - **E2-6, steps and pivots** (#283): the position, angle and scale steps in the tool bar
    beside the grid button, each a menu of values; pivots (the middle, each origin, the
    active entity) on the tool bar's right end and `.`; the session keeps both.
  - **E2-7, overlays** (#284): lights' and cameras' icons in the viewport, picked by a click
    or a box before the scene (a click again goes through to what lies under one); the
    selected ones' reach (a point light's range, a spot's cone, a camera's pyramid, the
    sun's direction); "View: Overlays" hides them. E2 is done; drops of assets onto
    surfaces come with E4's browser.
  - **E2b, the blockout** (ADR 0107 §7: brushes for greyboxing a level, their colliders
    and navigation mesh): the research notes (#285), `docs/research/blockout.md` (convex
    brushes as integer planes, exact predicates in `i128`, Godot's order over convex
    pieces, the pieces as hulls, the visible surface for navigation) and
    `docs/research/ux/blockout.md` (drawn on surfaces with B, typed sizes, face handles,
    Tab for faces, edges and corners, cuts shown dashed, Play at once, bake to a mesh).
  - **E2b-1, the core** (#286, ADR 0110, proposed: three questions to the team):
    `engine_brush`, convex brushes as integer planes with every side exact in `i128`
    (3.4 ns), Godot's order over convex pieces, the surface each brush owns, a `Group`
    kept evaluated through edits (a door dragged among 2,001 brushes 0.58 ms on one
    thread; from nothing 57 ms).
  - **E2b-2a, brushes in the player** (#287): `Brush` and `BrushGroup` in scenes (the shape
    and its size, the operation, the tint; edited brushes' planes in millimetres), the
    shapes (box, wedge, stairs, cylinder, sphere), each group evaluated in the player's
    frame where something changed, a mesh per brush with a grid texture (cracks at
    T-junctions closed by growing each polygon 0.05 mm); `docs/dev-guide/blockout.md`.
  - **The owner's notes after trying the editor** (#288): the camera's drags lock and hide
    the cursor; the tree's eye and H turn entities off with what is below them
    (`Disabled`, saved; not drawn, lights dark, brushes out); a game's `dev` build has the
    engine as in release and no validation layer (`ENGINE_VALIDATION=1`), and playing uses
    it too (`cargo run`). Small changes now go in batches with targeted tests (the owner,
    2026-10-21).
  - **E2b-2b, brushes in the editor** (#289, ADR 0110 accepted: rooms merge, the owner's
    snapping with inference, the format as proposed): the add menu in groups (Point,
    Mesh, Blockout, Light, Camera; "Empty" is "Point"), brushes into the selection's or the
    scene's blockout group, cones (pointed, truncated) and tori; snapping on G.
  - **Games share one build of the engine** (#290, ADR 0111; the owner asked for the
    engine compiled once, through a DLL): `ofeliaengine new` writes cargo's `build-dir`
    in the user's cargo folder and copies the engine's `Cargo.lock`. A second game's
    first build: 3.4 s, not 102 s; with the editor 4.0 s, not 20 s; its `target` 31 MB,
    not 1.9 GB. The DLL, measured: 0.03 s of a 1.5 s rebuild, and the game's own
    components lost (the reflected registry is a linkme slice per module); it waits for
    hot reload of game code.
  - **E2b-3a, drawing brushes** (#291): B on the surface under the pointer (the
    selection's too), Shift+B a cut; the first corner, the base, the height by clicks or
    typed sizes; R turns stairs and wedges; the brush live in its undo step, so a doorway
    opens as it is drawn; edges and sizes over it.
  - **E2b-3b, rooms** (#292; the team's answer: rooms merge): `Brush::hollow`, walls the
    shape's planes moved out; every room's inside takes itself out of every room in the
    group, whatever the order, so a corridor opens a hall's wall with no doorway cut;
    Ctrl+H for the selection and while drawing. 400 rooms and 380 corridors: 121 ms from
    nothing, a corridor dragged 1.45 ms.
  - **E2b-3c, side handles and landing** (#293; the team's answer 2): the active brush's
    sides dragged by handles, the opposite side staying; while drawing, corners land on
    other brushes' corners and edges within 8 px (a green dot, a red ring), before the
    grid.
  - **E2b-4a, colliders** (#294): a static compound per brush group, its pieces as sharp
    hulls (`ScenePhysics::sync_blockout`); moving bodies with `BodyDesc::internal_edges`
    roll on over the seams (measured: 0.00 mm, where rounded hulls or separate bodies knock
    3-8 mm); View: Colliders in the editor.
  - **E2b-4b, navigation** (#295): View: Navigation bakes the brushes' visible surface in
    the background a moment after each edit, on a pool of the editor's own, and draws the
    mesh; 400 rooms (357 tiles) in 158 ms on four workers. Games bake it from
    `Blockout::surface_triangles`.
  - **E2b-5a, editing faces** (#296): Tab over the viewport; a drag moves a face along its
    normal (the brush becomes planes, undo brings its shape back), Alt+drag pulls a brush
    out of it or pushes a cut in; rooms keep their walls.
  - **E2b-5b, edges and corners** (#297): in Tab's editing a drag moves a corner or an edge,
    the part made again as the exact hull of its corners (`shape::hull`): a box's edge
    dragged onto another makes a wedge.
  - **E2b-6a, exporting a group** (#298; the owner chose E2b-6 before E3): Blockout: Export
    to glTF writes a group's surface as a `.glb` beside the scene, a part per tint over the
    embedded grid, so Blender opens the greybox as it looks.
  - **E2b-6b, models from glTF files** (#299): a `Model`'s mesh and material from a glTF
    file's parts (by name or `#index`), its textures decoded with their levels in the
    frame they are first used; `.meta` files (`AssetMeta`) with the file's id.
  - **E2b-6c, Bake to Mesh** (#299): a group written beside the scene as `<group>.glb` and
    drawn from it by a model per tint under the group, as one undo step; its brushes, not
    drawn, keep colliding and making the navigation; Blockout: Unbake brings them back.
    E2b is done but for the cook's path, which waits for the one-button build (E14).
  - **E3-1, Play and Stop** (#300, ADR 0107's addendum: Play runs on the scene's world and
    Stop puts it back, measured 7.3 and 8.4 ms for 10,000 entities): F5 makes the game
    with its `start` on the scene, F8 puts everything back (spawned entities, added
    components and resources gone, changes and despawns undone, the play's undo steps
    forgotten). While editing the world stands (`Player::set_simulating`): a scene's
    tweens, timers and sounds no longer change it. A panic in the game's hooks stops Play
    and says its line.
  - **E3-2, Play's controls** (#301): F7 pauses (the world still, the game's frame hooks
    not called), Shift+F7 steps one tick (`Player::step_tick`), Shift+F5 plays from the
    editor's camera (the scene's camera put there, `PlayFrom` for the game's player),
    Ctrl+F5 restarts, Shift+F8 ejects (the editor's camera and tools on the running game,
    the game's keyboard and mouse off).
  - **E3-3, keeping changes** (#302): Ctrl+K keeps the selection's changes made while
    playing ("Kept" in the tree), Ctrl+Shift+K lists everything changed since Play (by
    component, made, despawned) to pick from; at Stop they go into the scene as one undo
    step, the game's runtime components left out, what it made becoming scene entities.
  - **E3-4, the game's own window** (#303): Play shows the game in a window of its own, the
    scene drawn once, there (`Player::open_view_window`: a second swapchain in the same
    submission), the viewport saying where; F7 and F8 work in it, closing it stops, eject
    brings the view into the viewport; "Play: In a Window or in the Viewport". Latency
    control goes off while one has been open (NVIDIA's driver crashed with Reflex beside
    a second swapchain). E3 is done but for the tree's marks on what changed.
  - **E4-1, the asset database** (#304, ADR 0108's as-built; E4 is assets, ADR 0107 §5,
    `docs/research/ux/asset-browser.md`): every asset file under the game's folder with
    its id (our headers, `.meta` beside foreign files, written when missing, ids found
    again by path or content), the index in `.ofelia/` (our own text file: 10,000 files
    read in 3.2 ms, scanned unchanged in 20 ms), followed live by the editor,
    `ofeliaengine assets [--json]`.
  - **E4-2, the asset browser** (#305): the FileSystem dock lists the project's folders and
    each folder's files from the database; `#` in the palette finds assets by path, a kind
    narrowing it (`#model:crate`), Enter showing it in the dock.
  - **E4-3, drag and drop from the dock** (#306):
    - a model rests on the surface under the pointer while it is dragged over the view (an
      undo step, the surface pick leaving it out, Escape takes it back), or goes under a
      tree row, or becomes a root;
    - a material file goes onto an object or a row, and any asset onto an inspector asset
      field;
    - the model's entities come from its glTF's outline (`engine_assets::GltfOutline`, the
      JSON alone).
  - **Decided (team decision, 2026-10-22):** meshes of several materials draw with their
    first; `Model` gets material slots, one entity per mesh, as in Unity and Godot (not a
    part per primitive). Done after E5 (ADR 0115, below).
  - **E4-4, glTF files load in the background** (#307, ADR 0108's as-built):
    - a loading thread with a pool below normal priority reads and imports files, puts
      meshes together (their vertices encoded too) and decodes textures with their mip
      levels;
    - models draw once their parts are made, and the frame after a scene is loaded waits
      for it;
    - FlightHelmet dropped while running: the worst frame went from 1,110 ms to 7 ms;
    - mip levels 1.1–1.9× faster, to the bit (ADR 0040's addendum).
  - **E4-5, files written anew are made again** (#308, ADR 0108's as-built):
    - the database's scan names them, and the editor tells the player
      (`Player::asset_changed`);
    - the old version is drawn until the new one is whole (a material with its textures);
    - an image makes the glTF files that read it again.
  - **E4-6a, the dock's tiles and image thumbnails** (#309, ADR 0112):
    - `engine_ui` draws pictures (`Style::image`);
    - the dock shows tiles sized with Ctrl+wheel;
    - images are made smaller on a thread of their own, cached in `.ofelia/thumbnails/`,
      and kept in an atlas of 64 cells.
  - **E4-6b, previews the engine draws** (#310, ADR 0112):
    - the frame renderer's views into images of their own format, their alpha kept (a new
      tone mapping entry point);
    - `Player::request_preview` / `take_previews`: one a frame, drawn at twice the side
      and averaged down, read back;
    - models' and materials' tiles in the dock.
  - **E4-6c, the inspector's asset fields as in Godot** (#311, ADR 0112): a square
    preview the engine draws (a mesh with its material, a material on a sphere) beside
    the asset's name; a click picks another through the palette, a drop puts one in.
  - **E4-7, renames, moves and references** (#312, ADR 0107's as-built):
    - F2 in the dock renames a file and a drop on a folder moves it; its `.meta` goes
      with it, and its parts' paths follow;
    - Shift+F12 finds where an asset is used: the open scene's entities, then the scenes
      and materials on disk.
  - **E4-8, textures through the derived data cache** (#313, ADR 0108's as-built, ADR
    0041's addendum):
    - a texture in `.ofelia/ddc` loads compressed (BC7, BC5); another shows uncompressed,
      is compressed on a thread below the loading's priority and swapped in once whole
      on the GPU;
    - FlightHelmet: shown from the cache in 31–34 ms (273–321 ms without), its textures
      272 → 68 MB; the first time, compressed in the background in 8 s.
  - **E4-8b, the content hash of the cache's keys** (#314, ADR 0113): XXH3-128 (twox-hash,
    MIT) as `engine_core::hash::content_hash128` for the keys of textures, panoramas and
    colliders' parts, 55–64 GB/s against FNV-1a's 1.13; FlightHelmet's keys 42 → 1 ms,
    from the cache 31 → 27 ms; the old entries computed once more.
  - **E4-9, import settings** (#315, ADR 0108's as-built): `AssetMeta` holds
    `texture.compression` and `model.scale`, written only when not the defaults; the
    player applies them (an image's own `.meta`, or its model's for images inside it);
    the dock's pick is shown in the inspector with its settings, an edit written at once
    and the asset made again. **E4 is done.**
  - **E5, prefabs** (ADR 0108's "Prefab instances"; the owner: any scene is a prefab):
  - **E5-1, instances in the format** (#316, ADR 0108's as-built, format 4):
    - `[instance]` places a scene, `[override <instance>/<entity>]` changes what it
      placed, `remove` takes a component or an entity away; ids inside derived from the
      instance's, paths through nested instances;
    - a single-rooted prefab's root is the instance's entity; several roots get a holder;
    - what cannot be placed is kept as written; saving writes what differs from the
      prefab;
    - the player loads scenes with their prefabs; saving and loading plain scenes as fast
      as before (6.56 / 9.90 ms for 10,000 entities against 6.47 / 9.68).
  - **E5-2, prefabs in the editor** (#317, ADR 0108's as-built):
    - scenes load and save through their prefabs, found by id through the asset
      database;
    - a scene dragged from the dock into the view or the tree is placed as an instance;
    - "Make Prefab" writes the selection's subtree to a scene of its own and makes it an
      instance of it (the root keeps its id, the rest derived; `History::change_ids`
      undoes it with the references); "Unpack Prefab"; the tree names instances.
  - **E5-3, overrides in the inspector** (#318, ADR 0108's as-built): an instance's
    entities measured against their prefab (`PrefabBase`, cached per outermost instance
    until the prefab's file changes), names and assets too; ↺ and Backspace put the
    prefab's value back; "Prefab: Revert Overrides"; the title names the prefab.
  - **E5-4, prefabs followed, overrides applied** (#319, ADR 0108's as-built):
    - a prefab file written anew is followed by the open scene's instances, in place
      (references kept), keeping what they changed: values merged three ways against the
      base kept, entities and components it added placed, those it dropped gone; at Stop
      while playing;
    - "Prefab: Apply Overrides" writes an instance into its prefab (values, added and
      removed components and entities); the others follow at once.
  - **The owner's notes, 2026-10-22** (#320):
    - brushes no longer flicker while edited: a mesh made again is freed only once the
      scene has been extracted since (a tick), models read again likewise (ADR 0110's
      note);
    - the game has the mouse from Play on, the cursor hidden; Shift+F1 frees it and gives
      it back, a click in the game gives it back (ADR 0107's note);
    - the game's window at least 1280 × 720, opened where it was left.
  - **E5-5a, scenes in tabs** (#321, ADR 0107's as-built; found 2026-10-22: the editor
    held one scene and could not open another):
    - an inactive tab is its scene's text, instances as their overrides, so it follows a
      prefab written anew by itself; switching a scene of 10,000 entities: shown again
      18.9 ms, left 10.7 ms;
    - one history across tabs: steps name their document, undo and redo show its tab
      first, a scene's unsaved state is its own;
    - Ctrl+O (the palette's scenes, a double click in the dock), Ctrl+N, Ctrl+W (no
      question: unsaved changes kept), Ctrl+Shift+T, Ctrl+Tab; the session keeps the tabs
      and the one shown.
  - **E5-5b, a prefab in a tab** (#322, ADR 0107's as-built): "Prefab: Open" and the
    inspector title's Open open the nearest instance's prefab as a tab, the entity
    picked selected there; Ctrl+S writes it, and the other tabs' instances take it when
    shown again (their text holds only overrides). **E5 is done.**
  - **Material slots** (#324, ADR 0115; team decision, 2026-10-22): `MaterialSlots` beside
    `Model` holds the materials of a mesh's further primitives; the player draws each
    further primitive on a child it makes (`Derived`: never saved, left out of undo,
    picks name the model), the renderer and the extraction untouched; a model dropped
    from the dock fills them from its glTF.
  - **E6-1, the Output** (#325, ADR 0107's as-built; `docs/research/ux/output.md`):
    `engine_log::LogReader` reads the ring since its last read without locks or
    allocation; the panel (Ctrl+J, its tab) folds repeated messages with a count, marks
    each Play, filters by level and text, follows the newest; its tab counts the errors.
  - **E6-2, errors next to their causes** (#326, ADR 0107's as-built): a row's text names
    its cause (an id, `path:line[:column]`, a file); a click selects the entity, shows the
    asset in the dock or opens the line in VS Code; a failed build's errors are rows.
  - **E6-3 and E6-4, Tasks and History** (#327, ADR 0107's as-built): a Tasks tab lists
    the background work (the build, assets, compression, thumbnails, navigation) with how
    far and how long; the scene dock's History lists every step, the current one marked,
    and a click goes to it.
  - **E6-5, the profiler's first panel** (#328, ADR 0107's as-built): the last 120 frames'
    CPU and GPU times as bars, the costliest CPU stages (`FrameReport`) and GPU passes
    (the frame graph's timestamps). **E6 is done.**
  - **E8-1, scripts in tabs** (#329, ADR 0107's as-built): `.luau` files open in tabs
    after the scenes' (a double click in the dock, "File: Open Script…", an Output error
    at a script's line), their code where the viewport is. The text is a line array
    (measured: a key 0.5 µs, a new line 15 µs in a 100,000-line file), carets several at
    once, VS Code's keys and mouse; Ctrl+S keeps the file's line endings; the session
    brings the tabs back with their unsaved text. engine_ui gained placed nodes and
    monospace text.
  - **E8-1c, Luau's highlighting** (#330, ADR 0107's as-built,
    `docs/research/luau-highlighting.md`): Luau's lexical rules, luau-lsp's colors; each
    line's start state kept and made again from the first line changed (60 lines cut in
    5.6 µs a frame).
  - **E8-2, find, replace, go to a line** (#331, ADR 0107's as-built): VS Code's find bar
    in a script's tab (as you type, Enter and F3, Alt+C, Alt+W, Alt+Enter every match a
    caret, Ctrl+H replace and Ctrl+Alt+Enter all as one undo step), Ctrl+G's `:line`;
    the tab's keys a context of their own.
  - **E8-3a, Luau's analysis built in** (#332, ADR 0107's as-built): Luau 0.736's Analysis
    vendored (`third_party/luau-analysis`) and built by `engine_luau_analysis` against
    mlua-sys's Luau libraries. Measured: 20.6 s cold once (cached across profiles), at
    most 3.2 MB in the editor, a 220-line module checked again in 0.16 ms. Modules in
    memory or on disk, require by string, definitions, findings placed by line and byte.
  - **E8-3b, Luau's findings as you type** (#333, ADR 0107's as-built): a checker thread
    given each tab's text 150 ms after it rests; findings stamped by tab and version,
    underlined, each line's first message after its text, counted in the code bar (a
    click goes to the next).
  - **E8-3c, completion and hover** (#334, ADR 0107's as-built): Luau's `autocomplete`
    after `.`, `:`, a word's first letter and Ctrl+Space, narrowed as the word grows,
    written with parentheses as one undo step; the mouse resting on a name shows its
    type.
  - **The freed mouse** (#335, ADR 0021's addendum): `DeviceFilter` filters the keyboard
    and the mouse apart; a game whose mouse is freed (Shift+F1, paused) stops hearing it
    and keeps the keyboard, and `set_devices` lets go of what it stops hearing.
  - **E10, materials: the UX note** (#336, `docs/research/ux/materials.md`). It studies
    Unreal, Unity, Godot, Blender and Roblox, and proposes E10's steps:
    1. materials in the inspector, edited live;
    2. Slang in the editor;
    3. the graph;
    4. previews and cost.

    It found that typed materials (ADR 0092's `.material.ron`) do not draw in the editor
    or the player, which read only ADR 0108's `.material`. Four questions are with the
    owner: one material file, variants with a base, numbers on graph nodes, where
    generated Slang lives.
  - **E8-4, go to definition, find in files, signatures** (#337, ADR 0107's as-built): F12
    and Ctrl+click across `require`s; Ctrl+Shift+F, the palette's `%` over the project's
    scripts; a call's signature above the line, the argument at the caret lit. E8 is done
    but for rename and references across the project.
  - **The frame cap by default** (#338, ADR 0023's addendum): a release build's player
    keeps a few frames under the display's refresh rate (`hz − hz²/3600`; 224 on ref-1's
    240 Hz, 4,463 µs frames against 2,003 uncapped), and as far over it with vsync, a
    backstop for a display that does not pace. `FrameCap`, `Player::set_frame_cap`; the
    cap follows the window to another monitor.
  - **E10-2a, Slang in the code editor** (#339, ADR 0107's as-built; research
    `docs/research/slang-in-the-editor.md`): `.slang` files are assets
    (`AssetKind::Shader`) and open in code tabs, with Slang's lexical colors. Slang's
    front end checks a tab as it is typed (`Compiler::check_source`): a material type
    inside `forward.slang`, as it compiles. A misspelt field shows at its place 0.28 s
    after typing.
  - **Rows dragged in the tree** (#340, ADR 0107's as-built; the owner's request): onto a
    row into it, near its edges before or after it, below the rows last among the roots;
    world placement kept (Alt: local), one undo step; an instance's parts stay.
  - **ADR 0114 accepted** (the owner, 2026-10-22: "да" to all seven recommendations): E7
    can start.
  - **Brushes inside brushes** (#341, ADR 0110's addendum; the owner's request): Godot's
    rule, a brush's children act on it first and what they make of it acts in its place
    (`Brush::made`, `csg::combine`). A window below a wall cuts that wall only. A shape
    added with a brush active goes inside it, a cut drawn on a brush into it. Nested 400
    rooms: a door dragged 0.63 ms (flat 0.59).
  - **A code tab's own panel** (#342, ADR 0107's as-built; the owner's request): Open
    Files and the file's Outline on the left of the code, the scene's tree and inspector
    aside (View: Scene Docks Beside Code keeps them). Text files open in code tabs too
    (TOML, RON, JSON, Fluent, Markdown, plain), listed in the FileSystem dock without ids.
  - **E7-1a, the script host** (#343, ADR 0114's as-built; `docs/dev-guide/scripting.md`):
    the project's `scripts/` loaded before any scene. `component(name, props)` registers a
    plain-data component with a `TypeInfo` made at run time. `on_start`, `on_tick` (before
    `Game::tick`), `on_frame` and `on_stop` run while the world simulates. `self` holds
    props and live views of the entity's components; `world.find`, `time`, `quat`,
    `print`. Measured: an empty hook 0.19 µs per instance (target 0.5); a view's access
    about 150–177 ns (target 100); 1,000 doors 3.3 ms a tick (target 1): views' fast
    path is E7-1c.
  - **E7-1b, script components in scene files** (#344, ADR 0114's as-built):
    `engine_ecs::RuntimeType`, a process-wide list of component types made at run time.
    `SceneRegistry::from_program` reads it (`Access::Dynamic`), so `Door.lift = 3` lines
    load and save in the player and the editor, and undo keeps them.
  - **E7-4a, scripts in the editor** (#345, the owner's request of 2026-10-22, ADR 0114's
    as-built): Add Component… (Ctrl+Shift+A, a button under the components), the × on a
    component's header, Attach Script… (the scene dock's script button), New Script… from
    a template named after the entity, loaded by the player's host and attached as one
    step. A script component shows its file under its header and a mark on its row; a
    click opens the script. `SceneRegistry::insert_default`, `Player::load_script`.
  - **E7-4b, a script reloads when it is saved** (#346, ADR 0114's as-built): Ctrl+S runs
    it again at the next frame, the file watcher when another editor saved it. Instances go
    on with their state; props that changed move by name to a new layout
    (`engine_ecs::Relayout`, `script::Door#2`), and the editor's history and Play's
    snapshot follow. 1,000 instances: 0.21 ms in place, 0.33 ms moved (target 100).
  - **The owner's notes of 2026-10-22** (#347): a transform's scale keeps its proportions
    while the lock beside its name is shut (the default, kept in the session); Ctrl+A
    opens the add menu under the pointer, inside the window.
  - **View modes, V1a** (#348, the owner's request of 2026-10-22, ADR 0116): Lit,
    Wireframe and Lit + Wireframe from the viewport's chip or Alt+4, Alt+2, Alt+5.
    `engine_render::wire` draws the edges after the frame in line mode, the scene's depth
    sampled against a tolerance, so lines never z-fight the surfaces they lie on. 10,000
    cubes: 0.41–0.99 ms on the GPUs, a lit view unchanged.
  - **View modes, V1b** (#349, ADR 0116): Unlit on Alt+3, a pass of its own
    (`engine_render::unlit`) drawing each material's base color and emission over a dark
    background; the forward pass untouched. 10,000 cubes: 0.28 / 1.15 ms on the GPUs.
  - **View modes, V2a** (#350, ADR 0116): the blockout's colliders (Alt+C) and the
    navigation mesh are lines in the world (`engine_render::wire::LineLists`), tested
    against the scene's depth as the wireframes are: faint behind surfaces, steady on the
    ones they lie on; the X-ray overlay is gone. 19,200 edges: 10 / 82 µs on the GPUs,
    41 µs of the CPU's frame.
  - **Colliders of a scene's models, step 1** (#351, ADR 0117; ADR 0116 V2b): a scene file's
    models had no colliders at all (only packs' did). `Assets::collider` now makes each
    model's as the cook would, by the rules beside its glTF (level geometry by default),
    on a cooking thread of its own when first asked; built-in meshes have their own
    shapes. The editor draws them with the blockout's, colored by motion.
    `engine_collision::Shape::edges` gives any shape's lines.
  - **Models' bodies, step 2** (#352, ADR 0117): `Game::before_ticks(scene)` and
    `ScenePhysics::sync_scene(scene)` give the blockout's brushes and the models their
    bodies in one line (`Game::tick` had only the world, so the guide's `sync_blockout`
    call could not be written there). Static models follow their entities, props fall
    and roll moving theirs; it visits only what changed and allocates nothing while
    nothing does.
  - **E7-2, the engine's types** (#353, ADR 0114's as-built): the editor's Luau checker
    knows the engine's API and the program's components (findings, completion and hovers),
    a hook's `self` typed from `component`, with Luau's old solver (the new one of 0.736
    leaves it unknown). The editor writes them to `types/ofelia.d.luau`, and `ofeliaengine
    new` points luau-lsp in VS Code at it.
  - **E7-1c, the views' fast path** (#354, ADR 0114's as-built): `self`, views and
    entities are tagged userdata whose metamethods are C functions on Luau's own API,
    fields found by their names' atoms; the time budget's interrupt is a C function that
    reads the clock every 64 calls. A prop's read and write 386 → 71 ns the pair, a
    field's 296 → 75 ns, 1,000 doors 3.4 → 0.65 ms (target 1 ms). Found: the shipping
    profile aborts on a binding's error (Luau's C++ exceptions through mlua's callbacks
    under `panic = "abort"`): ADR 0118 proposed `unwind` (+5 % executable, +3.5 % in
    mlua-heavy code).
  - **The shipping profile unwinds** (#355, ADR 0118 accepted: team decision of
    2026-10-22, option 1): a script's error stops its instance in what players get too.
    `check.ps1 -Profiles` runs a probe of three script errors built with `shipping`.
  - **E7-3a, physics queries for scripts** (#356, ADR 0114's as-built):
    `physics.raycast`, `physics.overlap_sphere` and `physics.filter` on the game's physics
    world (`Game::physics`) or the player's own (`PlayerSettings::physics`), which runs
    the pack and the scene's models and brushes for a game that has none. Bodies carry
    their entity in their user data (`entity_user_data`), so hits name entities.
  - **E7-3b, triggers and contacts** (#357, ADR 0114's as-built): a `Trigger` component
    (box, sphere, capsule) is a kinematic sensor body kept where its entity is
    (`ScenePhysics::sync_triggers`, part of `sync_scene`); scripts hear the last step's
    contacts as `on_touch(self, other, point, normal)` and `on_untouch(self, other)`,
    once per step. The editor draws triggers with the colliders, in yellow.
  - **E7-1d, entities as values** (#358, ADR 0114's as-built): an entity is a light
    userdata of its handle (equal by value, a table key, `typeof` `Entity`, never
    allocated); views through entities are kept in a table of 256 places; `world.find`'s
    index follows the names that changed; the host allocates nothing in a stage. The door
    example on 1,000 entities with ten watchers: 1,237,831 allocations in 600 ticks → 0
    (`tests/zero_alloc_scripts.rs`); an empty hook 0.165 → 0.138 µs, 1,000 doors 0.63 →
    0.6 ms.
  - **E7-5a, entities' methods** (#359, ADR 0114's as-built): `has`, `add`, `remove`,
    `despawn` (at the stage's end, with its children, after their `on_stop`), `children`
    (a loop that keeps nothing), `find_child` (the nearest), `parent`, on entities and
    `self`; `self:open()` calls `Door.open(self)`; components as values (globals of the
    program's, a script's `component` table, or a name); `Transform`'s `rotate`,
    `look_at`, `forward`, `right`, `up`. None of them allocates (the zero-allocation
    test's walkers).
  - **E7-5b, `require` between scripts** (#360, ADR 0114's as-built): Luau's require by
    string over the project's modules (mlua's require with the host's own navigation:
    `./`, `../`, `@self`, `.luaurc` aliases); what a script requires runs before it,
    whatever the files' names (its `require` paths scanned before loading); a reloaded
    script's dependents run again after it, and the editor names them.
  - **E7-5c, queries, systems, spawning** (#361, ADR 0114's as-built): `world.query(A,
    B)` with `with`/`without`, looped as `for e, a, b in q do` (a snapshot at the loop's
    start, the query's own views moved to each entity: nothing made per step);
    `system(name, { on_tick, on_frame })` after the instances; `world.spawn({ … })` at
    once. 10,000 spinners as a system 1.2 ms a tick (target 2), as hooks 2.7 ms; the
    spin example allocates nothing.
  - **E7-5d, input** (#362, ADR 0114's as-built): `input.pressed`, `just_pressed`,
    `just_released`, `strength`, `axis`, `vector` by action name, the tick's view in tick
    hooks and the frame's in frame hooks; C functions, so nothing allocates (ADR 0114's
    walker in the zero-allocation test).
  - **E7-5e1, messages and timers** (#363, ADR 0114's as-built): `e:send(hook, …)` (at most
    four plain values) delivered after the hook that sent it, up to 16 passes a stage;
    `self:after(seconds, hook, …)` at the start of a tick. Nothing allocates (the
    zero-allocation test's relays).
  - **E7-5e2, random numbers and sounds** (#364, ADR 0114's as-built): `random.number`,
    `integer`, `vector_in_sphere` from the world's generator (SplitMix64 as a component of
    its own entity: rollback and saves carry it, the same seed everywhere), C functions;
    `audio.play`, `audio.play_at` make entities that play once and despawn.
  - **E7-5e3, prefabs and assets in scripts** (#365, ADR 0114's as-built): an asset is a
    value (`prop.asset(kind)`, `AssetRef` fields on the fast path); `world.instantiate(
    prefab, translation?)` places a prefab scene by asset or path, its instances
    anonymous (no `SceneId`), their scripts started at the next stage.
  - **E7-5f1, script components in saves** (#366, ADR 0114's as-built): saves took script
    components but skipped them on load (`engine_save` looked up only the program's
    types): types made at run time load now. `after` timers ride on their entities into a
    save (a `ScriptTimers` component put on just for it), and `on_load(self)` runs at the
    next tick after a load.
  - **E7-5f2, the networking rules** (#367, ADR 0114's as-built): `net.is_server`,
    `is_client`, `resimulating`; a joined client runs its predicted entities' tick hooks
    (`Scripts::tick_predicted`), again on re-simulation (`engine_net::Resimulating`),
    sounds, messages and timers then left out. Scripting v1 is done with it.
  - **E10-2b, Slang's completion, hover, signatures and definitions** (#368, ADR 0107's
    as-built; research note's "slangd, probed again"): `slangd` as a child process, its
    own checks off. A material type is read inside `forward.slang`'s prelude, no shader
    changed; F12 goes into `forward.slang`. Completion 24 ms after a change, a definition
    15 ms.
  - **E7-6a, props of text** (#369, ADR 0114's as-built): a string prop is a `String` the
    component owns. Run-time types drop their texts (`DropGlue::Texts`), write defaults
    with them (`RuntimeType::write_default`, `DefaultValue` shared by saves, scenes, the
    inspector and undo), and move them on reload (`Relayout::take`). Read and written on
    the fast path with nothing made in steady state; ADR 0114's coin checks clean.
  - **E7-6b, script components in the network schema** (#370, ADR 0114's as-built):
    `NetSchema::replicate_script(name)`, found when a session starts; props' `net_quantum`
    and `net_skip`; a client's value starts from the script's defaults; the server reads
    columns that own texts. Prefabs from packs move to the one-button build (nothing
    packs scenes or scripts yet). Scripting v1 is complete.
  - **Prefab following takes script components** (#372): an instance now gets a script's
    component its prefab added (E5-4 made only the program's types, through their
    `Default`); its texts at the script's defaults (`engine_ecs::DefaultValue`).
  - **Fixed: a closed VM's queries corrupted a later VM's registry** (#375; from E7-5c):
    - a query collected as its VM closed queued its views' references under the VM's
      address;
    - a VM made later at the same address let go of them, which were entries of its own
      registry (hooks, views);
    - the script measurement (six VMs in turn) aborted in 6 of 7 runs (exit 3, wherever an
      entry was used next);
    - closing hosts now leave nothing queued (`vm_made`, `vm_gone`): 6 of 6 runs pass.
  - **E9's research and ADR 0119** (#374, proposed):
    - research `docs/research/script-debugger.md` (Luau's debug interface, six debuggers,
      DAP, how to stop while the editor draws on the same thread);
    - UX `docs/research/ux/script-debugger.md`, replacing the note of 2026-10-17;
    - ADR 0107 §6's fallback (frames drawn from inside a breakpoint) cannot be built: the
      frame cannot be entered again. ADR 0119 proposes the stage on a fiber, or a stage
      that can stop halfway; four questions to the team.
  - **E9-1a-1, the debugger's core** (#376, ADR 0119's as-built):
    - breakpoints placed through each module's chunk and kept through reloads;
    - logpoints and pause points, with conditions and hit rules, evaluated in the frame
      (its locals, upvalues and globals);
    - the host logs the messages at their lines;
    - debug level 2.
  - **ADR 0119 accepted** (#377; team decision, 2026-10-23, every recommendation):
    - a stop suspends the script stage on a fiber of its own;
    - the editor edits the world at a stop; where the stage holds its borrows (a stop
      inside a metamethod or a C library's callback), the world is read-only;
    - the VS Code extension is kept in the repository and installed from a `.vsix`; on the
      Marketplace and Open VSX at the first public release;
    - pause on errors is on by default in the editor, once per line per Play.
  - **E9-1a-2, logpoints and pause points in the editor** (#379, ADR 0119's as-built):
    - a breakpoints' column in the game's scripts' gutter: ◆ logpoints and circled ‖ pause
      points, hollow when off, a bar for a condition or a hit rule; a click, Shift, Alt,
      the middle and right buttons; F9, Shift+F9, Ctrl+F9, Alt+F9, Ctrl+Shift+F9;
    - an options line under the code: Log or Pause, the text, the condition, the hit rule;
    - a logpoint's latest message and hits after its line while Play runs; a pause point
      pauses Play after its tick;
    - breakpoints follow their lines through edits, kept in the session, given to the
      host on the lines of the code that runs (unsaved edits move the marks only);
    - until stops come, F9 and a click make pause points;
    - fixed: a breakpoint was reached once and lost on most lines (Luau's userdata
      instructions write their own opcode back); placed again after each hit;
    - measured: nothing without breakpoints; 0.6–1.4 µs a hit; F10 kept from the window
      menu.
  - **E9-1b-1, stops** (#380, ADR 0119's as-built):
    - fibers in `engine_platform` (one made once, `suspend`, `resume`, panics handed back);
    - `BreakAction::Stop`: the frame described (locals, upvalues, the stack, values never
      through metamethods), the host's handler waits with the call's budget held;
      `Resume::Abort` ends the call (`ScriptError::Aborted`) and the stage runs out;
    - the player's script stages on the fiber while debugging is on (the editor's Play): a
      stop holds the stage, the frames go on without ticks; Continue goes on with the
      frame's world opened anew, then the rest of the tick; Play's Stop ends it;
    - the stage holds no borrow of the world while it waits (open pointers read again
      after each hook, contacts copied out, the host boxed): the world is editable at
      every stop;
    - the editor: F9 and a click make ● breakpoints; at a stop the script's tab at the line
      with the arrow, an amber band and the frame's values, the viewport's edge amber, the
      mouse freed; F5 goes on, F8 stops Play; breakpoints changed at a stop reach the VM;
    - Tracy's zones of the stage closed at a stop and opened again after it (a zone 18.0
      -> 19.2 ns); the stage on the fiber costs nothing measurable (0.26 ms a tick of
      1,000 spinners either way).
  - **E9-1b-2, steps** (#381, ADR 0119's as-built):
    - hooks resumed on a Luau thread of the debugger's while a breakpoint that stops is set
      or a step is under way (0.12 µs a hook more; nothing without);
    - a step turns single-stepping on and breaks out of the interpreter, which is entered
      again in its stepping loop: over, into, out, and past a hook's end into the next;
    - `self`'s `__namecall` has a continuation, so `self:method()` can be stepped through;
      a stop under C code without one cannot be stepped from, and says so;
    - the editor's scripts compiled at Luau's optimization level 1 (no inlining: steps follow
      the text), measured free on ADR 0114's scenes;
    - F10, F11, Shift+F11 in the editor and the game's window.
  - **E9-1b-3, the Debugger panel** (#382, ADR 0119's as-built):
    - a bottom tab (Ctrl+Shift+D) opening by itself at a stop: Continue and the steps, the
      stop's hook and place;
    - the call stack (a caller's line and values a click away), the frame's locals and
      upvalues (every frame described at the stop), every breakpoint with what it does and
      its hits (its mark toggles it, its row goes to its line);
    - its texts made again only when the stop, the frame, the breakpoints or their states
      change.
  - **E9-1b-4, stops at errors** (#383, ADR 0119's as-built):
    - an error a script does not catch stops it at its line in Play, before its frames
      unwind (the hook's message handler, or the frames a failed resume leaves), once a
      line a Play; one a `pcall` catches when asked (Luau's protected-error callback);
      the time budget's error inside its loop;
    - the editor: the line and the viewport's edge in red, the message after the line,
      `Error · on_tick · …` in the Debugger, which errors stop as its list's first rows
      (kept in the session), "Break at the Next Script Line" in the palette;
    - measured: the handled call 14 ns a hook more at most; interpreted scripts 8-33 %
      slower (the question below);
    - fixed: Continue after a step inside native code entered it halfway through a block
      (an access violation); the stepping loop now runs the rest of the hook.
  - **E9-1b-5, values at a stop** (#384, ADR 0119's as-built):
    - the lines above the stopped one show what they assigned (`t = Transform`,
      `self.speed = 2`), the function's line its parameters; changed values lit;
    - the Variables open: tables (raw, the first hundred), `self` (props, components,
      entity), entities and components through reflection (the host's describer);
    - Evaluate in the shown frame (its own thread and budget, the world opened to it) and
      watches, evaluated at every stop, kept in the session;
    - F5, F8 and the steps work while a field is typed in.
  - **E9-2, VS Code over DAP** (#385, ADR 0119's as-built):
    - the editor's endpoint on 127.0.0.1 with a token in `.ofelia/debug/editor.json`, its
      own threads, requests answered in the editor's frame (breakpoints, error filters, the
      stack, scopes, variables, evaluate, the steps, pause) and its events (stopped,
      continued, the scripts' log in the console);
    - the extension (`tools/engine/vscode/ofeliaengine`): Attach to the editor, Attach and
      Play; `ofeliaengine new` writes `launch.json` and installs its `.vsix`.
  - **E9-3, the standalone player's debugger** (#386, ADR 0119's as-built):
    - `cargo run -- --debug` (`--wait-for-debugger` keeps the world still until VS Code is
      ready) in a development build; the endpoint moved to `engine_app::dap`, the requests
      about a stop answered alike for the editor and the player;
    - at a stop the window's title says where and the mouse is freed; what the client set
      goes with it;
    - the extension's "Attach to the player"; `engine_app::run` installs the engine's logger;
      the game template's shipping profile unwinds (ADR 0118).
  - **E9-4a, the stopped entity** (#387, ADR 0119's as-built): the host tells the VM
    whose hook runs; the Debugger's header names the entity and the tick; Shift+F7 finishes
    the stopped tick and pauses; "Only Front door" limits a breakpoint to an entity (kept by
    scene id in the session). Fixed on the way: the player counted the steps a still world
    skips as ticks.
  - **E9-4b, changing a value at a stop** (#388, ADR 0119's as-built): F2 or a second click
    on a Variables row gives it Luau; `self`'s props and components are an undo step; VS
    Code's Set Value in the editor and the standalone player.
  - **E9-4c, one set of breakpoints with VS Code** (#389, ADR 0119's as-built): the editor's
    breakpoints reach VS Code as `breakpoint` events, attaching joins the two sets, and VS
    Code's own list keeps the editor's options. The script debugger's plan (E9) is done.
  - **Scripts in the profiler** (#390, ADR 0114's as-built): a Tracy zone a hook
    (`script Door.on_tick`); the editor's profiler lists each script's time, calls and
    memory a frame while the game plays, measured only while the panel is open.
  - **The editor's changing texts allocate nothing** (#391): a text laid out anew (the
    status line's FPS, a counter) goes into a layout unused for two seconds, its string
    and glyph buffers reused; before, each change made a new string and glyph list. The
    editor's FPS follows a fixed frame time in its zero-allocation test
    (`Player::frame_time`), so a test of the text cache lays out 600 new texts instead.
  - **The RTX's baseline remade under its new driver** (#392): `forward` read 3.1-3.7 %
    slower in `sponza_fsr_quality` and `sponza_smaa_t2x` (2.8 % in `sponza_gi`) in three
    gates, `main` alone the third, on code those scenes do not run, registers unchanged.
    The baseline was taken on NVIDIA 616.92 (2026-10-15); ref-1 runs 617.42. Every NVIDIA
    row remade: the GPU frames moved by a median of +1.0 % (-1.2 to +3.7 %).
  - **E11, sound,** started (ADR 0107's order; E10-1 waits for the answer on the material
    file): its UX note `docs/research/ux/sound.md` orders the steps and asks four
    questions (below). **E11-4a, sounds in the viewport** (#393, ADR 0107's as-built):
    icons, the add menu's Sound group, a selected source's two distances as rings (dashed
    where the sound holds beyond them), a selected reverb zone's box and its fade.
  - **E11-3a, sounds' waveforms in the dock** (#394, ADR 0107's as-built): a sound's tile is
    its waveform, read once on the thumbnail thread (`engine_app::waveform`) and cached;
    3 minutes of Ogg in 188 ms, off the frame.
  - **E10-1a, one material file** (#395, ADR 0120): `.material` version 2 holds a type's
    instance (its shader and type, numbers, vectors and textures by field name, its look);
    the cook (`--project`) and the run time read it with the project's root; the six golden
    files converted; `.material.ron` retired.
  - **Full script debugging** (#396, ADR 0119's as-built): the Debugger's switch runs the
    scripts interpreted from the next Play, so a stop shows every frame's locals; off by
    default (native code, as the game runs them); kept in the session.
  - **E10-1b, typed materials drawn by the player** (#397, ADR 0120's as-built): a typed
    `.material` on a model draws its look at once, then its type once the forward
    renderer's material compiler thread has reflected its layout and compiled its entry
    points (1.6 s on the RTX, 1.1 s on the Radeon, no frame over 7.6 ms meanwhile); its
    images load in the background; thumbnails wait for the type's shaders.
  - **E10-1c-1, a material file edited in the inspector** (#398, ADR 0120's as-built): a
    `.material` picked in the dock shows its rows (a material type's from its reflected
    `Params`); edits draw by the next frame with nothing compiled, are steps of the one
    history (file steps: its text before and after) and Ctrl+S writes them.
  - **E10-1c-2, materials from the object and the material commands** (#399, ADR 0120's
    as-built): a selected object's material file shows its rows after its components,
    with how many objects use it; New Material, Make Variant (a copy with its own id) and
    Apply to Selection, each one undo step; unsaved material edits kept with the session.
    E10-1 is done.
  - **E11-3b, sound previews in the dock** (#400, ADR 0107's as-built): ▶ on a sound's
    tile, Space and the arrows after a click in the tiles, Auto Play kept in the session,
    a playhead with seeking, Escape; played as the file is on an editor bus of its own
    (`Player::preview_sound`), streamed from where it is asked; an Ogg file's length from
    its last page.
  - **E11-3c, a sound's waveform in the inspector** (#401, ADR 0107's as-built): a band per
    channel, read on a thread of its own and drawn at the size it is shown; ▶, ■ and the
    loop; a click or a drag along it plays from there; its length, rate and channels,
    peak in dBFS and silence at each end. E11-3 is done but for sounds dropped on the
    viewport, the tree or an entity, and the sound and bus fields.
  - **E11-1a, the mixer file** (#402, ADR 0121): the buses and faders in the project's
    `main.mixer` (buses keyed by name: parent, gain in dB, balance, voices, reverb; the
    master's gain and balance), the built-in four without it; the player lays its mixer
    out again live (`Player::set_mixer_layout`, and when the file is written), what plays
    going on; the game's `Mixer` volumes multiply the faders; `AudioSettings::buses` is
    gone.
  - **E11-1b-1, the mixer's strips** (#403, ADR 0107's as-built): the Audio tab (Alt+A), a
    strip per bus with its fader (room near 0 dB, one undo step a drag) and the picked
    bus in the inspector (name, output without loops, gain, balance, voices, reverb);
    + Bus and ×; each change a file step of the one history, heard by the player the next
    frame; Ctrl+S writes `main.mixer`.
  - **E11-1b-2, the mixer's meters and listening aids** (#404, ADR 0107's as-built): a bar a
    channel (−60 to +6 dBFS) read only while the Audio tab is open, held peaks and clip
    lights cleared by a click; M, S and Monitor kept in the session, on top of the game's
    mutes (a solo keeps its way to the master and what feeds it); the unsaved layout
    autosaved with the session.
  - **E11-1b-3, the mixer's changes in Play** (#405, ADR 0107's as-built): heard at once and
    kept after Stop, a dot on each strip changed since Play, one step "Mixer: changes in
    Play" after Stop, and a command taking them back. E11-1 is done.
  - **E11-2a, effects and sends in the mixer file** (#406, ADR 0121's version 2): each bus's
    (and the master's) effects in order, a kind of the twelve built-in by name with its
    parameters by name and unit, bypass and sidechain; sends keyed by the bus they feed;
    loops left out in one order; the player follows live, effects carrying on through
    moves (a reverb's tail rings on); `engine_audio` moves and bypasses effects, publishes
    gain reduction, and describes the built-in kinds.
  - **E11-2b, the mixer's effects and sends in the editor** (#407, ADR 0107's as-built): a
    section per effect in the inspector (parameters in their units, bypass, a filter's
    shape, a compressor's sidechain, ↑ ↓ ×), a section per send, Add Effect and Add Send
    offering only what would not loop; on the strip the effect's short name or count with
    the gain reduction now, and B. E11-2 is done but for the room's reverb strip.
  - **E11-4b-1, handles on sounds and reverb zones** (#408, ADR 0107's as-built): a square on
    each of a selected sound's rings (pointed at: "40 m · −16 dB, held"; dragged: its
    distance, the inner ring pushing the outer), a zone's sides (the opposite side
    staying) and its fade; one undo step a drag, Escape.
  - **E11-4b-2, hearing in place** (#409, ADR 0107's as-built): P plays the selected sources
    where they are, looping, heard from the editor's camera with the zones, until P, Escape
    or another selection; Sound: Listen to the Scene plays every source set to play;
    nothing written back (`Player::audition`, copies of the components). E11-4 is done but
    for View: Sound Ranges and the falloff curve.
  - **E11-3d-1, sounds dropped into the scene** (#410, ADR 0107's as-built): onto an object
    (viewport or row) it plays there in 3D; on the floor (or with Alt) a new entity named
    after the file on the surface; into the tree, heard as it is; one undo step each.
  - **E11-3d-2a, sound and bus fields** (#412, ADR 0107's as-built): a source's sound shows
    its file's waveform and ▶, takes a dropped sound, and opens `#sound:` (the arrows
    playing each file with Auto Play); its bus is a list of the mixer's buses, a name the
    mixer lacks marked. The marks: `Hint::Sound`, `Hint::Bus` (`#[reflect(hint = ...)]`).
  - **E11-3d-2b, renames follow the marks** (#413, ADR 0107's as-built): a sound moved in
    the dock renames the marked strings naming it in the world, in undo's and Play's
    records, in other tabs and in scenes and prefabs on disk (scripts listed, not
    changed); a bus renamed in the mixer takes the open scene's sources along in the same
    undo step and names the other scenes; Find References finds a sound's users. E11-3 is
    done.
  - **E11-2c, the room's reverb strip** (#414, ADR 0107's as-built, ADR 0121's version 3):
    "room (auto)" at the mixer's right end: the reverb's meter, what the room follows
    ("Hall 62 %", "the walls"), its fader a trim kept as `room.gain`; picked, the room
    heard now, read-only. E11-2 is done.
  - **E11-5a, snapshots in the file and the player** (#415, ADR 0122): named sets of
    values by path in `main.mixer` (version 4), layered by weight in the file's order,
    the later winning; `Mixer::blend(name, weight, seconds)` and Luau
    `audio.blend(name, weight, fade?)`; fades on the game's thread, nothing allocated a
    frame; the editor's mixer functions keep the paths through renames, moves and
    removals.
  - **E11-5b, the panel's snapshots** (#416, ADR 0107's as-built): the header's list (Mix,
    each snapshot, + Snapshot); a chosen snapshot shows over the mix and is heard alone,
    and the panel's value edits go into it (one step each, ↺ takes one out, buses and
    effects refused); its section names it and sets its fade. E11-5 is done.
  - **E11-4c, the last sound gizmos and the falloff curve** (#417, ADR 0107's as-built):
    View: Sound Ranges (every source's outer ring and every zone's box, dim); a 3D
    sound's level over distance in the inspector, the camera's distance in red with the
    level there; in Play, ejected, sources filled while they play and hollow while
    silent; the camera that hears without a listener marked with an ear; a selected
    zone's room and priority on a label; a zone added with a selection takes its box.
    **E11, sound, is done.**
  - **E10-2c, Material: New Type** (#418, ADR 0107's as-built): a material type from a
    template named after the active object (`water.slang`, `WaterMaterial`) and a material
    file of it given to the selection in one step, the module open at its surface; a
    type's new fields reach the inspector after its save through the reflection's
    revision (now tested in the frame's hot reload test). E10-2 is done.
  - **E12-1a, skinned models drawn skinned** (#448, ADR 0124's as-built): a scene's model
    whose mesh a skinned node of its glTF draws gets its skin made in the background
    (parts with their influences, the rig with the rest pose and the file's clips
    compressed: CesiumMan's in 36–41 ms), an instance per part, and its rest pose;
    `Player::model_rig` lists the clips and `Player::pose_model` shows one at a moment
    (0 allocations posing every frame, both GPUs). Material slots make a skin a
    primitive. `engine_anim` is now a dependency of `engine_app`.
  - **E12-5a, the `.agraph` file and transition conditions** (#476, ADR 0124's as-built):
    - `engine_scene::AnimGraphFile` holds parameters, layers, nodes (clips, 1D and 2D
      blends, machines), states and transitions, keyed by ids. Lines at their defaults are
      left out, and `problems()` names broken references.
    - `engine_anim::Condition` compiles `speed > 0.1 and grounded` to stack operations:
      checked without allocating, triggers listed, errors with their places.
    - E12-5's plan: 5b runtime, 5c player and scripts, 5d editor canvas, 5e live state.
  - **E12-4e-2, a character's joints posed in the view** (#475, ADR 0124's as-built):
    - A character's own clips are listed after its model's in the Animation tab. Choosing
      one shows its kind's tools, and New Clip is offered for characters too.
    - In Tab's joint mode the picked joint gets rings in its own axes. A drag turns it,
      keyed at the playhead as one step ("Pose <joint>"); Escape takes the drag back. K
      keys the picked joint.
    - 0 allocations with the rings laid out every frame while the clip plays. The clip's
      time text follows the chosen clip.
  - **E12-4e-1, joints keyed in `.anim`** (#474, ADR 0124's as-built):
    - Tracks `@<joint>.<part>` key a character's joints. Its animator's own clip poses
      them over the rest pose, and the joints found once per model, not searched each
      frame.
    - `play(clip, fade)` blends the poses across own and model clips; root motion fades
      out into an own clip.
    - The Animation tab's preview poses them; K keys them at the playhead.
    - Going from an own clip to a model's and back no longer allocates (the animator
      keeps its bindings): 0 allocations in 654 frames with such switches.
  - **E12-4d, curves** (#473, ADR 0124's as-built):
    - The Animation tab's Curves view (switch or C) draws the picked keys' tracks as lines,
      a color for each number, with the keys as points.
    - A point is dragged in time (whole frames, Shift freely) and in value, with the range
      held while it drags.
    - Smooth, Linear and Step set the picked keys' curves, each one undo step.
    - `engine_ui` draws lines through points (`Ui::line`); 0 allocations while the view
      plays.
  - **E12-4c, the dope sheet's editing** (#472, ADR 0124's as-built):
    - Keys on a clip made in the editor's tracks are picked by their marks (Ctrl adds) or
      by a box, and dragged by whole frames (Shift: freely).
    - Delete deletes them, Ctrl+C copies them, and Ctrl+V pastes them at the playhead.
    - The clip's length is typed in the header, and the tab's facts follow the file.
    - Each edit is one undo step.
  - **E12-4b-2, keys** (#471, ADR 0124's as-built):
    - The editor's frame runs with a previewed clip's fields holding its values, while undo,
      saves and prefabs record the scene's.
    - An edit of one of those fields keys it at the playhead's frame, in the edit's own step.
    - K keys the clip's fields (an empty clip: position, rotation, scale); ◆ beside an
      inspector field keys that one.
    - Text layouts come back sooner when few are spare (unused this frame and the last),
      64 kept at hand: texts that change every frame allocate nothing.
  - **E12-4b-1b, New Clip, renaming and deleting clips** (#470, ADR 0124's as-built): New
    Clip makes an `.anim` in the dock's folder for the selected entity (an `Animator` added
    when missing), one undo step; the header's name field renames a clip, ✕ takes it away.
    **Team decision (2026-10-30):** K keys the fields the clip moves (an empty clip:
    position, rotation, scale), ◆ keys one field, editing a moved field keys it.
  - **E12-4b-1a, clips made in the editor in the Animation tab** (#469, ADR 0124's
    as-built): an entity's own clips listed, played and scrubbed in the tab, its tracks
    shown with their keys, its events and Once edited in its `.anim`; the player previews
    the clip for drawing only (`Player::preview_anim`), putting the scene's values back
    before the editor, saves or undo read the world.
  - **E12-4a, clips made in the editor** (#468, ADR 0124's as-built): `.anim` files of
    keyed fields (one track per `Component.field`, below-entity paths, Euler rotations,
    smooth/linear/step keys, events, once), played by any entity's `Animator` through its
    new `clips` (name to file) each tick after the animators move on; bindings kept per
    animator in the animators' order, no allocation, 70 ns a door with two tracks.
  - **E12-3e, each clip's loop and root motion** (#467, ADR 0124's as-built): a clip's
    `.meta` entry says whether it plays once (the animator holds its end) and which of its
    root's motion root motion takes (across, up, turn). The Animation tab's header toggles
    them with undo. **E12-3 is done.**
  - **E12-3d, Mark Foot Plants** (#466, ADR 0124's as-built): the Animation tab's button
    (or the command) puts `footstep` events, `left` and `right`, where each foot touches
    down on the chosen clip, in place of the ones marked before; one undo step. Feet by
    name, else each named side's lowest joint.
  - **E12-3c, a joint's trail and speed curve** (#465, ADR 0124's as-built): a joint
    picked while the Animation tab shows a character's clips leaves a magenta trail
    through the clip in the view, a tick a frame. Its speed draws as a bar a frame under
    the events, the longest step lit and named; a click scrubs there.
    `Rig::joint_path` gives the path.
  - **E12-3b-3, event sounds in the previews** (#464, ADR 0124's as-built): the dock's model
    and P in place play an event's sound as they pass it, on the dock's preview bus. The
    model's events are held whenever its Clips show.
  - **E12-3b-2, the Animation tab's events** (#463, ADR 0124's as-built): marks on a strip
    under the ruler. E adds one at the playhead with its name ready to type, and a drag
    moves it frame by frame. The row under the strip edits the name, data and sound;
    Delete removes. Each change is written to the model's `.meta` as an undo step and given
    to the player. Also fixed: E12-3a's ruler was laid out 600 px wide.
  - **E12-3b-1, clip events** (#462, ADR 0124's as-built): a model's `.meta` names moments
    of its clips (time, name, data, sound). Each tick fires those its play passed (any
    speed, backwards, several loops; through a fade only the heavier clip) into
    `AnimationEvents` for the game's tick; scripts hear `on_animation_event(name, data)` in
    the next tick; a sound plays where the character stands. 17 ns a character a tick, 0
    allocations.
  - **E12-3a, the Animation tab's timeline** (#461, ADR 0124's as-built): Alt+N shows the
    Clips' chosen clip on a timeline, for the selected character or the picked model file.
    Its ruler numbers the frames and scrubs; ←/→ step frames, Home/End go to the ends,
    Space plays. It shares the Clips' playhead. 0 allocations while it draws.
  - **E12-1b-3b, two clips side by side** (#460, ADR 0124's as-built): a Ctrl+click on a
    picked file's clip plays it beside the chosen one in the preview, at the same fraction
    of its length, framed with it on the one floor. 0 allocations. The root's path on the
    floor waits for the Animation tab's trails (E12-3).
  - **E12-1b-3a, the preview's floor, In Place, the arrows** (#459, ADR 0124's as-built):
    a picked model file's preview stands on the blockout's grid. A clip that crosses the
    ground walks on its spot while the floor goes by under it; In Place holds the floor
    still. After a click in the Clips, ↓ and ↑ move through the clips. 0 allocations.
  - **E12-2c, root motion** (#458, ADR 0124's as-built): `Animator::root_motion` keeps a
    clip's ground motion in the pose (the default), moves the entity by it each tick
    (`Move`), or leaves it in `moved` and `turned` for the game's controller (`Report`).
    It is read from the skin's topmost joint against the file's up (glTF's +Y), blended
    through fades, and taken out of the pose. On both GPUs a generated walker crosses
    1.25 m in 80 ticks with its hips on the spot. 0 allocations.
  - **E12-2b, cross-fades and `animator:play`** (#457, ADR 0124's as-built):
    `Animator::play(clip, fade)` starts a clip and fades from what played. Both clips move
    on in the ticks; each frame blends the two poses by the fade's weight between ticks;
    the fade is saved. Scripts call `self.Animator:play("Run", 0.25)`. The Fox fades from
    Survey into Run on both GPUs, with 0 allocations while a fade goes on.
  - **E12-1c-3b, attaching in the editor** (#456, ADR 0124's as-built): with a character's
    joints showing (Tab), a tree row dropped on a joint's square or Skeleton row goes into
    the joint's socket (made for it where none is), keeping its place in the world, one
    undo step; a dropped character file's meshes under joints come in sockets. E12-1c is
    done.
  - **E12-1c-3a, sockets follow their joints** (#455, ADR 0124's as-built): a
    `JointAttachment` right under a character is a socket on one of its joints; each tick
    the player places it there (held pose, else the animator's, else rest) before
    transforms propagate, and what hangs under it follows (a cube in CesiumMan's hand, on
    the joint to 0 m). 0 allocations with a socket placed every tick, both GPUs.
  - **E12-2a, the `Animator` plays a clip** (#454, ADR 0124's as-built): an `Animator` on a
    character plays its clip, looped: its `time` moves on each tick (saved, so loads go on),
    each frame poses between the last two ticks, and the world standing still holds it. The
    editor's held poses win until let go. Its `clip` field lists the character's clips. 0
    allocations with a character playing, both GPUs.
  - **E12-1c-2b, Tab's joint mode and the Skeleton section** (#453, ADR 0124's as-built):
    Tab over the viewport on a character shows its joints as squares; the one under the
    pointer lights with its name, a click picks it (the selection stays). The inspector's
    Skeleton lists the joints as a tree, and a row click picks too. 0 allocations while a
    character plays in place with its Skeleton shown, both GPUs.
  - **E12-1c-2a, a character's joints in the viewport** (#452, ADR 0124's as-built): the
    selected characters' skeletons as white sticks in front of everything, moving with
    their clips (`Player::model_joints`, `Player::skinned_models`); View: Skeletons shows
    every character's in grey. 0 allocations while one plays in place, both GPUs.
  - **E12-1c-1, a dropped character is one entity** (#451, ADR 0124's as-built): a file
    that draws a mesh skinned drops as one entity, its first skinned mesh's, turned and
    scaled as the file stands it; its other meshes go under it at their places in the
    file; its joints and empty nodes are not entities (CesiumMan: 1 entity, not 23). The
    rest poses of the test characters fill their positions' boxes to the micrometre, so
    the drop stands them on the floor (checked on both GPUs).
  - **E12-1b-2, the engine's preview of a model picked in the dock** (#450, ADR 0124's
    as-built): `Player::set_model_preview` draws a skinned model alone, posed at a clip's
    moment and turned, each frame either changes (77 µs a frame on the RTX, 227 µs on the
    Radeon while it plays, nothing while it stands); a skinned model file picked in the
    dock shows its Clips under that preview, ▶ and Auto Play playing it. 0 allocations
    while it plays, both GPUs, after two fixes it found: the frame graph placing a shape
    again into a kept placement with less room (every one kept grows together now), and the
    text layout cache's spares.
  - **E12-1b-1, a character's clips and P in place** (#449, ADR 0124's as-built): the
    selected character's Clips in the inspector (frames, root speed, size, error, and the
    loop's seam: CesiumMan "jumps 3.1 cm at leg_joint_R_1", the Fox's within 0.2 mm), a
    clip held at a click, ▶/■, loop, speed and a scrubbing bar; P (now "Scene: Play
    Selection in Place") plays the selection's characters and sounds where they stand,
    nothing written. Clips are compressed within 0.1 mm in the model's units (the Fox's
    1.5–5.6 times smaller). 0 allocations in 240 frames playing in place, both GPUs, after
    two fixes it found: a selected model's per-frame list, and the outline's images made
    anew for every size of a moving selection (64-pixel steps now).
  - **E12's research and ADR 0124, proposed** (#447): the runtime of state machines and
    blend graphs read in Godot 4.7.2, Bevy 0.20.1, Fyrox 1.0 and ozz-animation 0.17 (ideas
    only; Unreal and Unity from their documentation), in
    [animation-graphs.md](research/animation-graphs.md); the tools of Godot, Unity, Unreal,
    Blender and Figma, with scenarios counted, in
    [ux/animation.md](research/ux/animation.md). ADR 0124 proposes an `Animator` playing a
    clip, a cross-fade or a `.agraph` graph, decisions in ticks and poses in frames, graphs
    cooked to flat arrays over a pose stack, and E12's steps; nine questions to the team.
  - **A material type's reload in one compile** (#446, ADR 0123's as-built): the entry
    points, the node previews' and the parameters' layout from one Slang request
    (`Compiler::compile_file_reflecting`; the editing build's module defines
    `MATERIAL_NODE_PREVIEWS`). A graph's code edit reaches the sphere and its tiles in
    0.39–0.43 s with previews on (0.73–0.77 s before), a new parameter in 0.43–0.48 s
    (0.62–0.68 s), on both GPUs: within the 500 ms budget.
  - **E10-4g-3, previews of function calls and in a function's tab** (#445, ADR 0123's
    as-built): `expand` gives where every call's outputs come from (`CallOutputs`,
    through later calls and inside functions); the editing build previews points (a canvas
    id, a port, a width), so a call shows what its first output gives at its width, and a
    function's tab (`Shown::Function`) shows its own nodes, inputs and outputs: on both
    GPUs a call's tile in 0.91 s, a function tab's in 0.78 s. **E10, materials, is done.**
  - **E10-4g-2, node previews on the canvas** (#444, ADR 0123's as-built): each node with an
    output shows its value in a 64-unit tile under its ports (a flat square, or a sphere
    where it reads the surface's place), drawn from the preview's build: in 0.84–0.88 s
    from opening the tab on both GPUs, a typed value on its tile in 5 frames, no compile.
    Requests are tagged, so an atlas is shown only under the nodes it was drawn for. Graph:
    Node Previews turns them off (kept in the session); 0 allocations in a graph tab's
    steady frames with them on. Left: function calls' previews and a function tab's
    (E10-4g-3).
  - **E10-4g-1, node previews in the engine** (#443, ADR 0123's as-built): the editing
    build's `node_preview` writes every shown node with an early return each (one compile
    a change of code, however many nodes); `nodePreviewFragment` (compiled for a graph's
    type with `MATERIAL_NODE_PREVIEWS`) draws them into an atlas of 64-pixel tiles, round
    where a node reads the surface's normal or place; `Player::request_node_previews`,
    drawn once no type is on its way (0.58 s Radeon, 0.60 s RTX from the request, the
    type's compile included). The editor's tiles next (E10-4g-2).
  - **E10-4f, a type's code and Draw as Fallback** (#442, ADR 0120's as-built): a typed
    material's **code** row gives the instructions and image samples its fragment shaders
    reach beside the engine's (`engine_rhi::spirv::code_size`; stripes: 805 and 3 against
    936 and 8, the same on both GPUs) and its last compile (0.40–0.54 s, one entry point);
    **draw as fallback** (a checkbox, and in the palette) draws it with its look, so the
    profiler's forward pass shows its type's cost over plain shading.
  - **E10-4e, a material shown while dragged** (#441, ADR 0120's and ADR 0109's
    as-built): a material dragged from the dock shows on the object under the pointer (or
    the row's) in no undo step, the own back when the pointer leaves it or on Escape; the
    drop gives it from the own as one step. The scene pick takes that object as opaque
    (`add_scene_pass`'s `solid`): a cut-away material otherwise went off and on every four
    frames as each pick saw through it.
  - **E10-4d, the material's preview** (#440, ADR 0120's as-built): at the top of its
    inspector section, drawn by the engine on the selected object's mesh (a sphere from the
    dock), redrawn on edits (values alone drawn once), turned by dragging.
  - **E10-4c, what a material costs** (#439, ADR 0120's as-built): the editor's device
    captures the driver's pipeline statistics (measured free: cold material pipelines
    532 vs 542 ms Radeon, 758 vs 768 ms RTX); a typed material's **cost** row gives its
    fragment registers per shading variant made beside the engine's own, spills and
    parameter bytes (stripes: 39 vs 47 VGPRs Radeon, 40 vs 56 RTX); a graph's bar the same
    of its editing build.
  - **E10-4b, a variant in the inspector** (#438, ADR 0120's addendum): ↺ on a variant's
    own values (against its base, putting the base's back), a Base row and "variant of …"
    in the title, Material: Apply to Base (one step writing the variant's values into its
    base, the variant left naming its base alone); variants follow their held bases in the
    editor's own frame.
  - **E10-4a, material variants** (#437, ADR 0120's addendum; the team's answer b): a
    material file may name its `base` and holds only the lines its base has not; every
    reader (player, cook, live files, inspector) reads it over its base, a held base as
    the editor holds it; cycles and pathless bases refused; the player makes a variant
    again when its base's file or held version changes (3 ms / 2 ms on the Radeon); Make
    Variant writes a file of `base` alone. The inspector's override marks are next.
  - **E10-3g-2b, the editor's functions** (#436, ADR 0123's as-built): `.mfunc` files in
    graph tabs (ports named in the inspector, an input's value now written to the file),
    previewed by their first output with unsaved edits; the project's functions in the
    node search (ranked best first while typing); calls following their functions' names
    and ports from the tabs or the files; Ctrl+G makes the selection a function beside
    the graph, Tab enters a call's function and back, Ctrl+Alt+G takes a call apart. Tab
    over a graph's canvas no longer toggles the viewport's face mode. **E10-3 is done.**
  - **E10-3g-2a, functions in the engine** (#435, ADR 0123's as-built; the team's `.mfunc`):
    a function's file is the graph's grammar, its input and output nodes its ports; a call
    keeps the function's file and ports; before writing, calls are made their functions'
    nodes (Unreal's inlining), what they write and what is wrong with them the call's; the
    cook, the player and the preview read functions alike, and the player makes a
    material again when its function changes. The editor's side is next.
  - **E10-3g-3b, the editor's `[Node]` functions** (#434, ADR 0123's as-built): the
    project's Slang modules scanned on a thread of their own once a graph is shown; their
    marked functions in the node search under their groups (one a node cannot call with
    why); calls titled by their marks; a call's signature following its function when the
    module changes; Slang's errors in a module at its first call.
  - **E10-3g-3a, `[Node]` functions in the engine** (#433, ADR 0123's as-built; the
    `.mgraph` file's version 3): Slang functions marked `[Node("Title", "Group")]` are read
    through Slang's reflection. A call node keeps the function, its module and its
    signature, so the cook and the player generate without Slang; the generated module
    includes the function's module and calls it, its out and inout parameters outputs.
    The editor's side is next.
  - **E10-3g-1, the Expression node and Slang's errors at their nodes** (#432, ADR 0123's
    as-built): Slang written in a node (`a` to `d`, `ctx`), a function of the type; Slang's
    error in the preview's module framed at its node with the message, kept while the code
    is. On the way: a module the editor writes is due at once (no stale error), and a
    type broken from the start is reflected again once fixed (it stayed unmade until a
    restart, hand-written types too).
  - **E10-3e-2b, frames** (#431, ADR 0123's as-built; the `.mgraph` file's version 2): C
    puts the selection in a frame, Unreal's comment box; its title bar drags it with the
    nodes inside it (by place, no list in the file), its corner resizes it, its title is
    the inspector's, Delete leaves its nodes.
  - **E10-3e-2a, reroutes** (#430, ADR 0123's as-built): a double-click on a link (or the
    node search) puts a reroute on it, a capsule in the colour of what it carries; it
    adds no code (a graph through reroutes generates the same module).
  - **E10-3f-3, errors on nodes and Show Slang** (#429, ADR 0123's as-built): a node that
    stops the graph (a loop, a kind or a port a file holds) framed in red with why under
    it; Show Slang opens the saved build in a read only code tab that follows the graph.
    Slang's own errors at nodes come with E10-3g's nodes whose code the user writes.
  - **E10-3f-2, the graph's preview** (#428, ADR 0123's as-built; the team's «а»): a
    sphere in the canvas's corner draws the graph as it is through its editing build; a
    number typed reaches it in 5 frames (34–60 ms), a link that changes the code in
    0.56–0.77 s. On the way: a type's reload due counts as on its way, a preview whose
    code is unchanged is drawn once (`PreviewRequest::same_code`, ADR 0112's addendum),
    and a preview waiting allocates nothing.
  - **E10-3f-1, values in the inspector** (#427, ADR 0123's as-built; taken before
    reroutes and frames): with a graph's tab shown the inspector shows the node selected
    alone (parameter name, a constant's value, unlinked inputs' own values, a texture's
    image, a note) or the graph's shading model; numbers dragged or typed, names checked,
    images picked or dropped from the dock, each a step (a drag one).
  - **E10-3e-1, the node search** (#426, ADR 0123's as-built): Space (Shift+A) over a graph's
    canvas opens the palette's list of node kinds at the pointer, its pick added there; a
    link dropped on the empty canvas opens it for the kinds that can take the link, the
    pick linked by the port that fits best; an image dragged from the dock onto the canvas
    is a texture node.
  - **E10-3d-2, New Graph, copies, carried links** (#425, ADR 0123's as-built): Material: New
    Graph writes a graph and a material file named after the object. The graph draws its
    glTF material's own look (maps where the project has their files, factors;
    `GltfOutline::maps` from the JSON), or the standard one. The file is given to the
    selection in one step and the graph opens. On the canvas, Ctrl+D copies nodes with
    the links into them; Ctrl+drag from an output carries its links to another output or
    takes them out. **E10-3d is done.**
  - **E10-3d-1, the graph canvas** (#424, ADR 0123's as-built): a `.mgraph` opens in a tab of
    its own (a double click in the dock), its canvas where the viewport is: boxes with
    their ports and values, links as curves, pan, zoom by steps, F; links made, lifted,
    moved and taken out by the mouse (loops refused), selection by click and box, moves,
    Delete, each a step of the one history; Ctrl+S, Ctrl+W, the session and autosaves;
    no allocation in a steady frame. Ctrl+Tab no longer moves the panels' focus.
  - **ADR 0123 accepted** (#423; the owner's answers, 2026-10-28): graph files are `.mgraph`
    (header `[mgraph version=1 id=…]`); graphs and shaders open as files in tabs of their
    own, as code does, not in a panel under the viewport; the bottom bar's Shader
    placeholder is gone.
  - **E10-3c, graphs drawn by the player** (#422, ADR 0123's as-built): a material file
    naming a `.mgraph` resolves to the graph's module, generated into
    `.ofelia/cache/graphs/` (written when it changed), its type `GraphMaterial`, the
    graph's numbers and texture images under the file's values; the player reads the
    material again when the graph changes and hot reload compiles it (on screen in
    0.6 s on the RTX, 0.5 s on the Radeon); the inspector hides `_` fields.
  - **E10-3b-2, the `.mgraph` file** (#421, ADR 0123's as-built; its proposed shape, the
    recommendation): read and written in the editor's grammar, defaults left out, nodes
    by id and links by input, sorted; a texture node's own image; `.mgraph` files known to
    the asset database with their id inside.
  - **E10-3b-1, the material graph's node library and generator** (#420, ADR 0123's
    as-built): `engine_material_graph`: 40 node kinds as Slang snippets with typed ports,
    the graph in memory, the generator (one module a graph, each function its own nodes,
    Slang's type rules, a variable for a value read twice, literals when saved and
    parameters while open, each line's node); every kind and model checked by Slang inside
    `forward.slang`, 73 types in one module.
  - **E10-3a, the material graph's design** (#419, ADR 0123 proposed; research
    `docs/research/material-graph.md`): a graph is a material type in a `.mgraph` file of
    the editor's grammar, generated into one Slang module by one crate for the editor, the
    player and the cook. Question 3 measured: numbers read as parameters cost up to +12
    registers on the RTX and +10 VGPRs on the Radeon (a toon's lit variant 63 → 72), so
    unnamed numbers are compiled in at save. Two questions to the team (below).
  - **The RTX's `video_player` golden image re-taken** (#418): on `main` it read a mean FLIP
    of 0.0103–0.0108 (the limit 0.01) under NVIDIA 617.42, taken under 616.92; the
    Radeon's matched exactly, the picture's content the same (the error spread thinly over
    the scene's textures, none on the video). The new image against itself: 0.0007–0.0021.
  - **Test runs keep their windows out of sight** (#411; the owner's request, 2026-10-28):
    `tools/check.ps1` sets `ENGINE_HIDDEN_WINDOWS=1`, and every window `engine_platform`
    makes is then cloaked (composed, its swapchain presenting, never on the screen), with
    no taskbar button, shown without being activated; uncloaking and bringing it to the
    front do nothing. `-ShowWindows` lets a run's windows be seen.
  - **Next** (the owner's order: materials first of the tools, then animation and 2D;
    E10 is done): E12, animation, by [ADR 0124](adr/0124-animation-graphs.md) (accepted
    2026-10-29, every question as recommended). E12-1 and E12-2 are done (layers, masks and the
    animator's other script calls come with the graphs, E12-5, and the clips' events, E12-3,
    as decision 1c has it; the root's path on the preview's floor with E12-3's trails); next
    E12-5b (the graph's runtime in `engine_anim`), then 5c to 5e; E12-4 is done.
  - **Asked (2026-10-30), for E12-5c:** the scripts' calls on a graph (the public scripting
    API): a) `animator:set("speed", 3.5)`, `animator:trigger("jump")`, `animator.state`,
    `animator:in_window("hit")` (recommended: one call each, about 0.1 µs, names cached); b)
    calls by type (`set_number`, `set_flag`); c) parameters as fields
    (`animator.params.speed = 3.5`: two calls a write, about 0.2 µs, a proxy object per
    character or an allocation per access, and a Luau type per graph for completion).
  - **Settled (2026-10-29, team decisions, every recommendation):** ADR 0124's nine
    questions on animation: 1c (an `Animator` plays a clip, a cross-fade or a graph), 2a
    (decisions in ticks, poses every frame), 3c (cross-fade or inertialization per
    transition), 4a (sync by the cycle's fraction; markers with the timeline), 5b (joints,
    events and number fields), 6a (one entity per skinned model), 7a (`.meta`, `.anim`,
    `.agraph`), 8a (no record mode), 9b (the last 10 s of play kept, in E12-6).
  - **Settled (2026-10-28, team decision):** E11-5's snapshots are layers with weights,
    as FMOD's and Wwise's: each snapshot overrides only what it names, several at once,
    the later in the file's list winning where they meet; scripts blend one in and out
    (`audio.blend("underwater", 1.0, 0.5)`, a weight over a fade), one snapshot alone being
    a blend to 1 (the sound UX note's question, answer b).
  - **Settled (2026-10-28, team decisions, every recommendation):**
    - E11's four (`docs/research/ux/sound.md`): the bus layout in a `.mixer` file named
      by `PlayerSettings::audio.layout` (`buses` goes; the four built-in buses without a
      file); snapshots in that file as E11's last step (scripts reaching them signed off
      with the API); mixer edits made in Play kept, marked, one command takes them back;
      sounds named by their path, the field marked as a sound so renames follow;
    - native code in the editor's Play: a switch, off by default (native, as the game
      runs), on for every frame's locals at an error stop;
    - E10's four (`docs/research/ux/materials.md`): one `.material` file (`.material.ron`
      retired); variants by a `base` field in E10-4, a copy first; a graph's numbers
      parameters while it is open, compiled in at save; generated Slang only in the cache
      and packs.

- **Done: the quick start** (queue item 4, #249; CLAUDE.md §16):
  - `docs/dev-guide/quick-start.md`, from installing to a cube on screen, then your own
    scene; an index of the guides;
  - `ofeliaengine new <dir>` starts a game: its manifest on the engine, the engine's
    build settings and toolchain, the quick start's game, a sounds folder;
  - `engine_app::prelude`: what a first game uses, from one crate;
  - `examples/quick_start.rs` is the game, rendered offscreen by a test so the guide
    cannot rot;
  - measured on ref-1: the new game's first build (the whole engine) 74 s, a rebuild after
    an edit 2.5 s.
- **Queue item 4** (the core's gaps for a game) is done with it.

- **Done: networking in the player** (queue item 4, ADR 0106, #248).
  - **The player runs the session;** the game talks to it through the world's `Network`
    resource:
    - `host`, `invite` (text: a connect token), `join`, `leave`;
    - the host's pawn, pawns given to joining players;
    - events, and the schema's one-shot events.
  - **Hosting:** the server runs around the game's ticks, and the router's port is mapped
    in the background.
  - **Joined:** the client runs the frame's ticks (`Runtime::frame_driven`): predictions
    and re-simulations through `Game::tick`, remote entities interpolated.
  - **Offline:** nothing (no loopback).
  - **Test:** a host and a guest in one process over UDP. The guest joins by invite,
    gets its pawn and predicts it; events go both ways; leaving is heard.
  - **Not yet:** hole punching and relays (a game's rendezvous), dedicated servers.

- **Done: sound in the player** (queue item 4, ADR 0105, #246).
  - **Sound is components:**
    - `AudioSource` plays a file under `sounds/`, as it is or placed at its entity;
    - `AudioListener` marks who hears (the main camera without one);
    - `ReverbZone` gives a place a reverb of its own.
  - **Saves keep every sound's position,** so after a load music and sounds go on from
    there (`restart_on_load` starts one over).
  - **Through the world** (the owner, 2026-10-16: both automatic and zones), with the
    game's physics given by `Game::acoustics`:
    - walls muffle what is behind them;
    - the room around the listener sets the reverb from 32 rays.
  - **One mixer for the player,** the videos' sound included. Its device opens on a
    thread of its own with the first sound. The world's `Mixer` sets the buses' volumes.
  - **The mixer:** clips start at an offset, voices say where they are, and streams are
    sought off the game thread (`PreparedStream`).
  - **Cost on ref-1:** 17 µs a frame for 1,000 moving sounds, 51 µs with the acoustics.
  - **The owner's follow-ups (same day, #247):**
    - sound files from a folder while developing (a changed file heard at once) and a
      sound pack in a release (`ofeliaengine sounds build`, pack version 10);
    - `AudioSource` plain, `AudioSource3D` for space.
- **Done: save games** (queue item 4, ADR 0104; the owner moved them before audio,
  2026-10-16, and chose both encodings and integrity by default).
  - **Done, steps 1 and 2** (#244):
    - reflection's strings, lists, optional values and maps;
    - the program's list of reflected types;
    - `engine_save`: the capture in the frame (0.51 ms per 10,000 entities), the binary
      and text encodings, reading by field name, `apply` to a reloaded level, files
      swapped in atomically with a `.bak`.
  - **Done, steps 3 and 4** (#245):
    - in the player: quicksave, quickload, slots in `Saved Games` and autosave, written
      behind the frame, through the world's `Saves` resource;
    - hooks, the level's stable ids (pack version 9), engine components saved;
    - physics in two lines; timers and tweens; a game's own saves;
    - `ofeliaengine save dump` and `save info`, `docs/dev-guide/saves.md`.
  - **Left:**
    - video positions (sound positions done with ADR 0105);
    - thumbnails;
    - GPU buffers saved on request;
    - signing.
- **Done: navigation** (queue item 4, ADR 0103, 2026-10-16, #243): Recast and
  Detour in `engine_nav`; the cook bakes the level by default (Sponza in 0.03 s, 24 KB),
  pack version 8; `Player::load_navigation` puts a `NavMesh` into the world; paths in
  microseconds (`docs/dev-guide/navigation.md`).
  - **Then a pause** (the owner, 2026-10-16: usage limits). Next after it: audio in the
    player, then save games (the owner's call on the format).
- **Done: 8b, video playback** (ADR 0100, 2026-10-16): AV1 in WebM decoded by the GPU's
  video engine through Vulkan Video (rav1d where the GPU cannot), the picture converted
  into a texture for materials and the UI, the Opus sound as the clock; games open videos
  through the player (`docs/dev-guide/video.md`).
  - Video with transparency (an alpha channel) is not part of it: the owner deferred it
    (2026-10-15; §6, Video).
- **Done: virtual shadow maps** (item 5, ADR 0093): steps 3a–3c on 2026-10-15, 3d on
  2026-10-16 (#237, `feat/vsm-transparent-sss`). `ViewSettings::virtual_shadows` turns them on
  (off by default).
  - **Everything a view with pages shades reads them**, and it draws no cascade:
    - transparent surfaces mark their own pages and look them up;
    - subsurface materials take their front from the mask and their transmission from
      the pages.
  - Faces turned to the light (subsurface and double-sided casters) no longer shadow
    themselves.
  - **Cost against the cached cascades** (Intel's Sponza, 1080p):
    - the RTX: 0.18 ms more still, 0.30 ms more walking, 39 µs less with 8 movers;
    - the Radeon at the integrated defaults: 1.25 ms more walking.
  - **The default for discrete GPUs stays off** (the owner, 2026-10-16): the cached
    cascades cost 0.18–0.30 ms less on the RTX.
- **Done: 8c, colliders made from meshes** (ADR 0101), but for the rules file and
  `RayHit`'s surface (queue item 3).
  - The owner, 2026-10-16: the rules in `<scene>.colliders.ron` beside the glTF,
    documented in the dev guide; by default every mesh collides as level geometry.
  - **Done (first PR):**
    - `engine_collision` (hulls, fitting, scale rules);
    - compound, scaled and baked shapes in the physics API;
    - colliders in the cook from glTF physics, `UCX_` names and the defaults;
    - pack version 6, baked for Jolt;
    - `ScenePhysics` in the player.
  - **Done (second PR, #231, 8c-3):** the Rust decomposition after CoACD's algorithm, in the cook
    for props and dynamic glTF bodies, cached in the DDC: CoACD's hull counts (but the
    mug's: 24 against 21) in 0.02-3.0 s per model where CoACD takes 1.4-195 s.
  - **Done (third PR, #232, 8c-4):** the owner's requirement tested: concave props scaled every
    way (mirrored and sheared included) dropped onto Sponza come to rest, and a ball falls
    into a U's notch; a rendered demo (`props_drop`); meshes made of several pieces now
    remeshed (a table of boards was cut as a solid running through itself).
- **Done: 8d, localization** (ADR 0102; the owner kept Fluent `.ftl` as the source format,
  2026-10-16).
  - **First PR (#233, `feat/localization`):**
    - the research note and the ADR (8d-1);
    - `engine_loc`: Fluent's syntax compiled, formatted without allocating, plurals and
      numbers by ICU4X (8d-2);
    - strings packs, `ofeliaengine loc build` and `loc check`, and the player's text from a
      pack or a live `loc/` folder in the player's or Windows' languages (8d-3).
  - **Second PR (#234, `feat/bidi-text`, 8d-4):** right-to-left and mixed text in `engine_text`.
    UAX #9 is adapted from unicode-bidi to buffers kept between calls (it allocated on every
    call), and all 91,707 of Unicode 16's conformance cases pass. Also font stacks per
    language and a golden image.
- **Next:** the third review's queue (`docs/research/core-audit-3.md`, §5 below), accepted
  by the owner on 2026-10-16.
  - Done: its P0 (#236: the local lights' shadows' bookkeeping bounded, a CPU stage check
    in the perf gate) and item 1 (VSM's leftover, #237).
  - Item 2 done: the budget counts the allocator's free room (#238), and Reflex's pacing
    is measured and its late wakes cut (#239, ADR 0023's addendum).
  - Item 3 done: the colliders' rules file, what a hit is made of, layers by name (ADR 0101
    8c-2b). Next: item 4, the core's gaps for a game.
  - The owner (2026-10-16) wants a layer for Claude Code to make games with the engine
    simply: queue item 5a.
  - **Reflex's tail is the driver's** (616.92): its sleep is never under ~1.5 ms, in
    0.5 ms steps, so above ~500 FPS frames hold at 2 ms. Reflex still halves input to GPU
    done there (1.97 against 3.63 ms) and stays on by default.
  - The owner wants results soon, without leaving the plan: gate what a change needs and
    skip runs that add nothing.
- **Parked:**
  - Leftovers of finished items, listed under them in §5: mesh shaders on the RTX (4,
    step 3), the player's follow-ups (8a), the iGPU's expensive passes (6b), memory (7a).
  - Candidates beyond the queue: §6.
- **After the base** (team decision, 2026-10-20: quality of life for now): terrain,
  water, foliage, HLOD and impostors, VST3 hosting. Queue item 10.
- **Done since 2026-10-12:**
  - custom materials (3c) and meshlets (4);
  - cached shadow cascades (5, steps 1–2);
  - post effects (6a: grading, depth of field, cinematic motion blur);
  - the perf gate's register check (6c) and anisotropy as a player setting (6d);
  - shared transient heaps (7a);
  - the player crate `engine_app` (8a, PRs #215–#220) and color vision settings (8e).
- **Questions open with the owner:** none. Settled 2026-10-28 (team decision, the owner's
  «а»): a material's preview while its graph is shown is a sphere in the canvas's corner,
  its numbers live. Settled 2026-10-28 (team decision): ADR
  0123 accepted, its files `.mgraph`, graphs and shaders opened as files in tabs of
  their own (no panel under the viewport). Settled 2026-10-22 (team decision): E10's
  four on materials (one material file, variants with a base, numbers on graph nodes,
  where generated Slang lives). Settled 2026-10-23 (team decision): ADR 0119's four, every recommendation.
  Settled 2026-10-22 (team decisions): ADR 0114's seven, every recommendation; Luau (E7)
  comes in the queue's order (E5-5, material slots, E6, then E7), its public API drafted
  meanwhile for the owner to sign off; meshes of several materials get material slots on
  `Model` (queued after E5); ADR 0108's prefab root rule stays; the Radeon's CPU check
  baseline is not refreshed for now (§6).
  Earlier: 2026-10-16, the virtual shadow pages stay off by default on discrete GPUs; the
  monitor was lent for exclusive full screen (§6).
- **Agents** (team decision, 2026-10-20): at most 2 at a time, when there is a need, for
  tasks that need no full test runs (research notes, surveys, drafts). Gates and merges
  stay the coordinator's (§2).

## 1. What matters most

The owner's goal is a **technically ideal engine core**. That means well-designed
internals and speed proven by measurement on both reference GPUs: the RTX 4060 Ti and the
weak 2-CU Radeon iGPU (`ENGINE_ADAPTER=radeon`). The editor, Luau and tooling come after
the core. The detailed rules are in `docs/agent-rules.md` ("What matters most" and the team
decisions). In short:

- **Decide by evidence.** Research first (papers, talks, shipped engines, open code with
  licenses and patents checked), then options in an ADR, then a measured choice.
- **Never make existing things worse.** Golden images, the per-pass perf gate and the
  register rule exist for that. A feature a view does not use costs it nothing, not even
  shader registers.
- **Responsiveness is first-class** (§7): no added latency without a measured reason.
- **Standing team decisions:**
  - no patented algorithms;
  - NVIDIA DLLs are never shipped;
  - no camera motion blur in gameplay;
  - singleplayer has no loopback client/server;
  - every antialiasing and scaling mode exists, and the game decides what players may
    pick.

## 2. How the work is run

- **Roles.** One coordinator session plans, writes agent briefs, gates and merges, and
  reports to the owner in Russian.
- **Agents.** At most **2** at a time, all on Opus (team decision, 2026-10-20), for tasks
  that need no full test runs: research notes, surveys, drafts. They write into the
  coordinator's scratchpad, and the coordinator reviews what they wrote before it is
  committed. An agent on code works on one task in its own worktree under
  `H:\Desktop\ofeliaengine-wt\<name>`, building in its own `C:\cargo-target\<name>` (the NVMe
  drive, ADR 0077), and ends with a PR. The main checkout builds in `C:\cargo-target\ofeliaengine`
  (`.cargo/config.toml`).
- **Gate.**
  - The coordinator merges after its own `tools/check.ps1 -Gpu`, plus `-Perf` when the
    change can move the gate's scenes (a SPIR-V their pipelines are built from, or code
    they run, changed; team decisions, 2026-10-21, ADR 0062) and `-Deny` when dependencies
    change. Scripts,
    examples and docs get light checks: `-Affected`, or the touched crates' own.
  - The checks take a machine-wide GPU lock and wait for a quiet CPU before measuring.
  - Before a merge, the branch has origin/main merged in.
- **After a merge.**
  - Remove the worktree (look for links first: never junction the main checkout's
    folders, give scenes via `ENGINE_TEST_ASSETS`) and its target dir.
  - `tools/clean.ps1` frees stale build outputs, after every merge. A few days of work
    leave most of the garbage within its default window of 3 days, so run it with
    `-Days 1`.
    - It does not go into the nested tracy and loom build directories: give each one
      alone (`-Targets C:\cargo-target\ofeliaengine\tracy`; several in one `-File`
      argument do not parse).
    - What it keeps (each crate's newest build, anything written in the last day)
      rebuilds nothing the last day's builds used.
    - After a build of everything (a change to the hack crate or to a profile, a new
      toolchain), `-Days 0.05` removes what that build did not write.
    - On 2026-10-18 it took `C:/cargo-target/ofeliaengine` from 168 to 37 GB:
      incremental caches 94 GB, superseded crate copies, the tracy directory 22 GB.
    - Five days without it took `C:\cargo-target` to 188.5 GB, and `-Days 1` freed
      131.6 GB (2026-10-23).
- **Backups.** Everything is pushed: agents push after every commit. When the usage limit
  stops agents, the coordinator snapshots their uncommitted work to `backup/wip*`
  branches, then resumes them by message.
- **Knowledge graph** (team decision, 2026-10-20): a map for agents new to a subsystem.
  graphify (`/graphify`, output in the main checkout's `graphify-out/`, not committed)
  holds the code by its syntax and the documents' concepts from an LLM pass.
  - Rebuilt by hand, not by git hooks: they rebuilt it after every commit and merge, CPU
    the perf checks then wait out. `graphify update .` refreshes the code, then
    `tools/graphify_post.py` (8 s; `--recluster` after an LLM pass) adds references
    between documents and crates and mentions tied to the code they name, drops noisy
    guessed edges, and names communities from the anchors in `tools/graphify_labels.json`.
  - An LLM pass over the documents reads their outlines (`--outlines`, about a third of
    the text): the ADRs, the research notes and the dev guide cost about 1.2 M subagent
    tokens on 2026-10-20.
  - Measured: for exact questions (who calls a function) search is faster and right; the
    graph links by name and misses method calls. It is for orientation.
- **Machine.** ref-1 needs VDDCR_SOC at 1.25 V, or the Radeon flips LDS bits under CPU load
  (`docs/research/radeon-igpu-lds-faults.md`). `check.ps1 -Gpu` runs that self-check first.

## 3. What is done (merged to main)

Estimate of 2026-10-30 (after E12-4b-1): the technical core about **94 %** done, the editor
about **80 %** of its fifteen phases (E1 to E11 done but E8's rename; E12, animation, about
60 %: characters, clips, events, the timeline and `.anim` files done, keys, curves and graphs
left; E13 2D and E14 the rest ahead), Luau about **88 %** (scripting v1, the debugger with
VS Code and the scripts' profiling done), the whole engine about **90 %** (2026-10-28, after
E9-1a-2: 94 %, 63 %, 70 % and 84 %; 2026-10-22: 94 %, 57 %, 45 % and 80 %;
2026-10-20: 93 % and 66 %; 2026-10-17: 92 % and 61 %; 2026-10-11: 74 % and 50 %). What comes after the base (queue item 10: terrain, water,
foliage, HLOD and impostors, VST3 hosting) is not counted.

Done since 2026-10-17:
- the quick start;
- HDR output in the player;
- the editor's foundation (E1) but for autosave, the session and the restart after a
  rebuild: scene formats 2 and 3, scenes in the player, the editor on the player,
  editing with undo, text fields, the command palette, the selection, multi-editing,
  the game's `main`.

Done from 2026-10-11 to 2026-10-17:
- custom materials, meshlets, cached and virtual shadows, post effects;
- the player crate, video, colliders made from meshes, localization;
- the third review's P0 and its device memory and Reflex items, the colliders' rules
  file;
- input, navigation, save games, sound and networking in the player.

Left in the core: recording on the job system.

| Area | State | Where |
|---|---|---|
| Platform, core, jobs, memory, timers, input, latency | solid | §5, §7 |
| Vulkan RHI: bindless, render graph, async compute, pipeline cache, crash/GPU-fault reports, Vulkan instances on a process-lifetime loader thread | solid | ADRs 0006, 0055, 0061, 0065 |
| Persistent GPU scene, ECS extraction, runtime frame loop, cameras | done | ADR 0064 |
| **The player** (`engine_app`): the device, swapchain, latency control and frame loop in one place; window modes and exclusive full screen; lights, skinned meshes, reflection probes and the GI volume as ECS components; features per view; an offscreen mode for tests | done | ADR 0099 |
| **Frame renderer** (`engine_frame`): one object composes a frame: scene → hook → exposure → TAA/TAAU/FSR/DLSS → bloom/tonemap; defaults per device class | done | ADR 0072 |
| Lighting: PBR forward with clustered lights, CSM, IBL, GTAO with bent normals and GTSO, SSS, transparency (sorted + OIT) | done | ADRs 0045–0047, 0054, 0058, 0059 |
| Custom materials: material types in Slang (`IMaterial`), material files, cooking, hot reload; shading models lit, unlit, clear coat, cloth, hair, toon and glTF's sheen | done | ADR 0092 |
| Meshlets: made by the cook, culled per meshlet (sphere, cone, depth pyramid) for views and shadow cascades, on by default | done (mesh shaders left) | ADR 0085 |
| Decals through a decal buffer before lighting | done | ADR 0082 |
| Local-light shadows (cached atlas), contact shadows (Bend) | done | ADR 0071 |
| The sun's shadow cascades kept across frames: scrolled with the camera, only exposed strips and moved casters' regions drawn | done (virtual pages in flight) | ADR 0093 |
| Reflections: parallax-corrected probes, SSR (FidelityFX SSSR-based) | done | ADR 0073 |
| Atmosphere (Hillaire 2020) and height fog | done | ADR 0069 |
| Color grading: white balance, channel mixer, shadows / mid-tones / highlights, ASC CDL, contrast, vibrance, saturation; with the tone curve in one LUT built on the GPU when either changes | done | ADR 0094 |
| Depth of field: FidelityFX DOF's tiled gather (near and far fields), after the upscalers; a thin lens | done | ADR 0095 |
| Motion blur on cinematic views only (`ViewKind::Cinematic`; Guertin et al.'s reconstruction filter) | done | ADRs 0067, 0096 |
| Color vision settings (simulated or corrected) and anisotropic filtering as player settings | done | ADRs 0097, 0098 |
| HDRI environments: `.hdr` and OpenEXR panoramas as the sky, their sun found and lit as the directional light, cooked into BC6H cubes (DDC, packs) | done | ADR 0083 |
| GPU particles and GPU radix sort | done | ADRs 0080, 0081 |
| Acceleration structures for ray queries: a bottom level per mesh (compacted), the top level written on the GPU from the GPU scene | done | ADR 0087 |
| Global illumination (**Echo GI**, the working name; the code's `ProbeGi`): a volume of probes traced with ray queries, converged at load, then a ray budget per frame, read at a reduced resolution (Sponza at 1080p: 0.33 / 2.4 ms per frame on the RTX / iGPU); measured against AMD's Brixelizer GI, built from the SDK as the reference: 4.5 / 14 times less GPU time, 29 / 18 times less memory, a physical bounce where Brixelizer GI gives a fifth | done | ADRs 0088, 0089, 0090 |
| Upscalers: TAAU, FSR 3.1, DLSS via Streamline (user-supplied DLLs) | done | ADRs 0060, 0063 |
| **Frame generation**, off by default: FSR 3.1's interpolation built from the SDK and shown by the engine's paced present thread (exact spacing; 60 → 120 frames shown for +8.3 ms on the RTX), the UI handed over as a layer and drawn on every frame shown (screen, world and camera canvases, nothing to mark); DLSS FG through Streamline (213 → 427 shown/s uncapped on the RTX, 60 → 120 capped; needs Windows' GPU scheduling, else a hint for the player) | done | ADR 0091 |
| Every antialiasing and scaling mode: MSAA 2/4/8× (+ alpha to coverage), FXAA, MLAA, SMAA 1x/S2x/T2x/4x, nearest/bilinear/Catmull-Rom/Lanczos-3/integer/FSR 1, TAAU/FSR/DLSS; `check()` and the GPU's support list for game menus | done | ADR 0075 |
| Skeletal animation, clip compression, IK, GPU skinning | done | ADR 0068 |
| Physics API on Jolt/Box2D: queries, characters, rollback | done | ADR 0066 |
| Networking: encrypted UDP transport with NAT traversal; replication, prediction, lag compensation, co-op example; in the player: host, invite, join through the world's `Network`, the client running the frame's ticks | done (hole punching through a rendezvous, dedicated servers left) | ADRs 0070, 0074, 0106 |
| Assets: glTF, cooking, packs, IoRing streaming; GPU memory budget, texture streaming by GPU feedback, mesh LOD streaming (`engine_streaming`) | done | ADRs 0056, 0057, 0076 |
| **Video**: AV1 in WebM on the GPUs' video engines (Vulkan Video; rav1d where they cannot), converted into a texture for materials and the UI, Opus sound as the clock; `Videos` in the player | done (packs, HDR, transparency left) | ADR 0100 |
| **Save games**: everything reflected saved without listing it, binary or text, integrity and a `.bak`; quicksave, quickload, slots and autosave in the player, written behind the frame | done (thumbnails, GPU buffers, signing left) | ADR 0104 |
| **Sound in the player**: `AudioSource`, `AudioSource3D`, `AudioListener` and `ReverbZone` components, positions saved; walls muffling and the room's reverb from rays through the physics world; one mixer for sources and videos; sounds from a live folder while developing, a sound pack in a release | done | ADR 0105 |
| Audio, text, 2D, basic UI | solid / basic | §11, ADR 0015 |
| Tests: golden images (FLIP, per vendor), per-pass perf gate, determinism, zero-allocation tests | solid | ADR 0062 |
| Editor | E1 to E9 done (E8, the code editor, but for rename across the project); E10, materials, done (the graph, ADR 0123); E11, sound, done; E12, animation, under way (ADR 0124, accepted: skinned models and the Clips done); E13 and E14 ahead | ADR 0107 |
| Luau | scripting v1 done: components and hooks, queries and systems, physics, input, messages and timers, saves, networking, reload on save, types; the debugger begun | §10, ADRs 0114, 0119 |

## 4. In flight

Nothing beside the editor's queue (2026-10-22): the coordinator takes its items in §0's
order, E5-5 next, one PR at a time. The records of what was here before (localization,
colliders made from meshes, video playback, meshlets, custom materials) are under their
items in §0 and §5; what is left of 8c (the rules file) is queue item 3.

## 5. Queue (in order)

**What is left, in order.** The numbered items below keep their history; struck-through
items are done.

The third review's queue (`docs/research/core-audit-3.md` §5), accepted by the owner on
2026-10-16 (A to both of its questions: the core's gaps before item 9; terrain, foliage,
water and impostors stay in §6):

1. ~~**5's leftover, required**~~ (done 2026-10-16, ADR 0093 step 3d): transparent
   surfaces and subsurface materials from the virtual shadow pages, no cascades beside
   them. They stay off by default on discrete GPUs too (the owner, 2026-10-16).
2. ~~**Device memory** (audit 3 §3.3) and **Reflex's pacing** (§3.4)~~ (done 2026-10-16).
   - The budget counts the allocator's free room, and the larger empty block goes (#238:
     the flight's rest of the process 688 → 360 MiB on the RTX).
   - Reflex (#239, ADR 0023's addendum): every submission of a frame carries its present id. The
     wait for the driver's signal is cut at three frame intervals (late wakes of 15–43 ms
     become 7 ms). The flight times input to GPU done (`--input-latency`).
   - The wider tail is the driver's: its sleep is never under ~1.5 ms. Left: a pacer of
     the engine's own above ~500 FPS, and exclusive full screen's hang in a session
     without the foreground (§6).
3. ~~**8c's rules file and `RayHit`'s surface**~~ (done 2026-10-16, #241, ADR 0101 8c-2b):
   - `<scene>.colliders.ron`, documented in the dev guide's physics page; per mesh by name
     or `#index`;
   - every mesh level geometry by default;
   - what a hit is made of on demand (`surface_of`: glTF material indices, about 7 ns a
     call, nothing when not called);
   - collision layers by name (`PhysicsSettings::layer_names`, the rules' `layer`, glTF
     collision filters).
4. **The core's gaps for a game** (audit 3 §4): navigation; save games; audio, input and
   networking in the player; a dev-guide quick start.
   - ~~Input in the player~~ (done 2026-10-16, #242): `PlayerSettings::input`, an
     `engine_input::Input` resource of the world. It reads raw keyboard and mouse input and
     XInput pads, is sampled before every tick and before `present`, and is released when
     the window loses the focus.
   - ~~Navigation~~ (done 2026-10-16, #243, ADR 0103): Recast and Detour in
     `engine_nav`, baked by the cook by default (pack version 8), the world's `NavMesh`
     resource through `Player::load_navigation`. Later: crowds, obstacles, off-mesh links.
   - ~~Save games~~ (done 2026-10-16, ADR 0104): steps 1-2 (#244, `engine_save`), 3-4
     (the player, the CLI, the guide, #245).
   - ~~Sound in the player~~ (done 2026-10-16, #246, ADR 0105): sources and the listener as
     components, saved; occlusion and the room's reverb from the physics world; zones.
     Then (the owner's choices, #247): `AudioSource` and `AudioSource3D`, the sounds folder
     kept live, sound packs.
   - ~~Networking in the player~~ (done 2026-10-17, #248, ADR 0106): the player runs the session
     the game asks for through the world's `Network` resource; the client runs the frame's
     ticks. Later: hole punching and relays through a game's rendezvous, dedicated servers.
   - ~~The dev guide's quick start~~ (done 2026-10-17, #249): `quick-start.md`,
     `ofeliaengine new`, `engine_app::prelude`, the quick start's game tested offscreen.
5. **9:** editor tools, Luau bindings and debugger, other tooling. **Designed 2026-10-17**
   (ADR 0107, ADR 0108; the owner's choices are in §0). The phases (ADR 0107 §7; as of
   2026-10-22, E1 to E5 done, §0):
   - **E1:** foundation: the editor linked into a game, the player split so the editor
     drives it, scene format 2, the tree and the inspector, commands, sessions, rebuild
     and restart;
   - **E2:** viewport tools; **E2b:** blockout (CSG brushes, the owner's addition);
     **E3:** Play on a copy of the world; **E4:** assets (ids,
     cache, import, browser; material slots on `Model` after E5, team decision
     2026-10-22); **E5:** prefabs; **E6:** output, errors, history;
   - **E7:** Luau in the game (bindings, `.d.luau`, script components; the public API to
     the owner); **E8:** the code editor; **E9:** the debugger (core, DAP, panel);
   - **E10:** materials; **E11:** sound; **E12:** animation; **E13:** 2D; **E14:** the rest
     the engine has (navigation, physics, lighting, video, localization, saves,
     networking, input, profiler, the one-button build).
5a. **A layer for Claude Code** (the owner, 2026-10-16: "a convenient layer so that Claude
    Code can be connected as simply as possible and make games by itself, without
    difficulties or crutches"). Design it with 9; the likely parts:
    - every step headless from the CLI, with machine-readable (JSON) output: cook, build,
      run offscreen, capture frames, compare golden images, read logs and the profiler's
      numbers;
    - a game project template with its own `CLAUDE.md` and a dev guide written for an
      agent too;
    - text formats an agent edits safely (they are already RON and Fluent);
    - an MCP server into the editor and the player: inspect the scene, spawn entities, play
      and pause, take a screenshot, read errors with their places.
6. Recording on the job system (§3.5); the Radeon's forward and TAAU, a shadow redraw
   budget (§3.6).
7. Housekeeping: ~~the probes test's warm-up (§3.7)~~ (fixed 2026-10-23, §6), CLAUDE.md
   §2 (ADR), the forward renderer split by feature (§3.9).

Before it: ~~8d, localization~~ (done 2026-10-16; the owner kept Fluent).

**Parked inside finished items.** Each waits for a measurement or the owner:
- 0: sccache for agents, the Radeon's variance;
- 0b: the fast-turn perf test;
- 3: GI's leftovers;
- 3b: frame generation's leftovers;
- 3c: pipeline warm-up order;
- 4: step 3, mesh shaders on the RTX;
- 6b: the iGPU's passes;
- 7a: memory;
- 7c: the `declare` split;
- 8a: the player's follow-ups.

Merged with the second review's findings (`docs/research/core-audit-2.md`). The owner's
answers of 2026-10-10: faster checks first, then the review's P0 fixes; the player crate
after the rendering queue; custom 3D materials after GI; the discrete AO default stays High.
The owner's answers of 2026-10-16:
- colliders' rules in `<scene>.colliders.ron`, documented in the dev guide; every mesh
  collides as level geometry by default;
- localization's source format stays Fluent;
- after VSM's leftover, the core's gaps before item 9, without leaving the plan (but
  without runs that add nothing);
- terrain, foliage, water and impostors stay in §6 until a game asks.

The owner's answers of 2026-10-15:
- no object motion blur in gameplay views;
- the virtual shadow pages after the player crate;
- a register check in the perf gate, as a warning (6c, done the same day);
- DOF's ring merging stays off for now, as an option later;
- anisotropic filtering as a player setting (6d, done the same day).

The owner's answers of 2026-10-11: decals through a decal buffer (ADR 0082); our own GI,
held to beat AMD's Brixelizer GI in cost (ADR 0084); meshlets first (ADR 0085); mimalloc
(ADR 0086); Radeon GPU Profiler installed on ref-1 (`D:\SDKs\AMD`); NVIDIA's shader cache
cleared; contact shadows made deterministic if it is affordable; the iGPU's +1 % from 0f
kept for now, to fix later; colliders (8c) made from meshes; the split of item 6 was the
queue item (done 2026-10-10); frame generation out of the box (DLSS FG and FSR FG, the HUD
never interpolated, immersive UI unmarked) right after GI, off by default.

0. ~~**Faster checks, first after the review**~~ (owner, 2026-10-02, confirmed 2026-10-10;
   done 2026-10-10 but for the leftovers at the end). Then
   `check.ps1 -Gpu -Perf` took 10–25 min, and it grew with every crate and test. Per
   step, from the runs before 2026-10-10:

   | Step | Minutes |
   |---|---|
   | test | 1–6.5 |
   | tracy | 2–6.5 |
   | gpu | 2–7 |
   | perf | 2.5–4 |
   | clippy (cold) | up to 5 |

   Measure every step before and after each change, then:
   - **remove duplicates:** the tracy step reruns the whole test suite (build with Tracy
     and run only the profiler's tests instead); the test step reruns GPU tests that the
     gpu step runs on both GPUs;
   - **link faster:** lld-link (closes ADR 0001), `line-tables-only` debug info for tests,
     fewer integration-test binaries per crate;
   - **share compiled dependencies across worktrees** with sccache (Apache-2.0);
   - **adopt cargo-nextest;**
   - **add an "affected crates" mode** for agents' intermediate runs. The full run stays
     the gate before every merge.

   Done (ADR 0077 and its addendum): duplicates removed, one build per check, nextest,
   `-Affected`, doc tests, docs and formatting for what changed, one test binary per crate
   (100 → 45), test builds with line tables only, ADR 0001 accepted (rust-lld measured
   ~12 % faster than link.exe), the build directory on the NVMe drive
   (`C:\cargo-target\ofeliaengine`: rebuilding every test after a change in engine_core
   40-58 s → 11-15 s): 632 → 141 s with nothing changed; the perf step (ADR 0062 addendum:
   runs a round at a time, a scene with a slower pass measured again, 100 frames per run,
   baselines from median runs, a 12 s wait for a quiet CPU instead of 2 × 2 min) 749 →
   237 s: the gate `-Gpu -Perf` 901 → 400 s. On 2026-10-23 (#378) a package's own build
   was found making copies of most of the workspace's builds in 88 of 96 cases (seven
   dependencies' features that the drift check missed, and Cargo's "debug info when
   possible" for what build scripts use). The hack crate and the dev profile fix them:
   0 of 96. Left: sccache for agents' worktrees (when
   agents run again), and a study of the Radeon's variance:
   - the shadow pass (and bloom) read 20–27 % slower in a few scenes for minutes at a time,
     in every round, with the CPU quiet, then normal again (2026-10-10). Work on the CPU
     is not it (it costs 10 % while every core spins and nothing seconds later). The slow
     scenes followed scenes with large render targets in the gate's order (FXAA after
     MSAA 4×, reflections after FSR 1 twice, FSR 1 after SMAA T2x), and the same scene
     measured again after itself read normal: the first suspect is where the driver places
     a new device's render targets in the iGPU's memory after the previous device freed
     large ones. Later the same day most scenes read slower for a whole check (frame
     22.1–23.7 ms over the day, memory-bound passes +15–37 %) with the CPU at 0 % and the
     RTX idle, and the same scene measured again after itself stayed slow: more likely the
     SoC's memory or Data Fabric power states. The owner chose (2026-10-10) a 10 %
     threshold for AMD and not counting scenes whose GPU ran slower as a whole (ADR 0062
     addendum) over a BIOS experiment (DF C-states, JEDEC memory speed), which stays
     possible;
   - `sponza_streamed` read +10 to +23 % against its baseline from run to run in the
     morning while `sponza` stayed within 0.5 % (the same with and without PR #120's
     streaming fix; within 1 % in the afternoon's runs): which levels are resident when
     the timed frames start depends on timing. Make the scene settle to a deterministic
     residency before timing.
0a. **The review's P0 fixes** (§3.1 and §3.3):
   - ~~`engine_rhi::Pipeline` without its copy of the device's function table~~ done
     2026-10-10: `Pipeline` 1,504 → 24 bytes (an `Arc` of the device), the renderers
     181/186 KB → 4 KB, no boxing needed; `engine_frame`'s test builds and renders every
     feature and antialiasing mode on a 1 MiB thread on both GPUs (it overflowed before;
     it fits 256 KB now); `game_loop` runs on the Radeon; the 16–64 MiB threads in
     `perf_gate`, `aa_bench` and `flight` are gone;
   - ~~GPU pass times without overlap~~ done 2026-10-10 (ADR 0011 addendum): a pass's
     time starts at the previous pass's end on its queue when that came later; the
     Radeon's texture feedback reads 88 µs instead of 2.4 ms, its culling passes
     0.3–1.6 µs instead of 4–76; both perf baselines re-measured.
0b. ~~**No visible streaming pop-in on fast camera turns**~~ (done 2026-10-10 but for the
   leftovers below; owner, 2026-10-02: in UE, models
   and textures failed to load in time when the camera turned quickly). Our ADR 0076 has
   the same risk:
   - texture levels come only from the current view's feedback;
   - mesh levels are asked only by instances in the frustum;
   - request → upload takes about 0.35–0.5 s.

   Done (2026-10-10): the Graphine patents read (`docs/research/streaming-fast-turns.md`:
   360° feedback views besides the main view read on US11195250 and are out, as is
   distance prediction combined with feedback); the fast-turn test (`stream_bench --turn`,
   `TextureStreamer::sharpness`): on first sight a few percent of the pixels wait
   0.08–0.15 s, later turns 0 %, tight budgets blurry all the time rather than after turns;
   mip fade (ADR 0078). Left:
   - ~~mesh levels by a sphere around the camera~~ done 2026-10-10: `LodView::around`
     (distance in every direction; after a fast turn on Sponza no mesh level is left to
     load, instead of 41, for 0.4–0.8 MiB more; the reference `flight` uses it);
   - the fast-turn test in the perf gate, and a larger scene than Sponza (its pack fits the
     disk's cache);
   - one feedback pass with a wider field of view instead of the main view's, if a larger
     scene shows pop-in.
   The review's additions, done 2026-10-10: the geometry pool reserved from the pack's
   totals as the scene opens (`GeometryPool::reserve`; the Radeon's worst frame at the
   first fast turn 5.0 → 1.7 ms), the upload ring grows ahead (`UploadRing::grow` from the
   frame renderer, `stats()`; a refused frame still drops its view once). Growth beyond a
   reservation (scenes added while running) still happens on the frame thread.
0c. ~~Spatial upscales Lanczos-3 (with an anti-ringing clamp) and bicubic Catmull-Rom,
   measured in ADR 0075's table. No Lanczos-2: FSR 1's EASU covers it (owner,
   2026-10-02).~~ Done 2026-10-10 (ADR 0075's addendum): both in the tone mapper,
   clamped to the four nearest texels (no dark rings around HDR highlights), each texel
   read on its own (the bilinear-fetch trick is ATI's patent); Lanczos-3 in two passes.
   Catmull-Rom 67 %: +0.04 ms RTX, about +1 ms Radeon, sharper than bilinear and better
   than FSR 1 in every metric; Lanczos-3 the same quality for about +2 ms on the Radeon.
0d. ~~**Motion vectors out of the forward shading** (review §3.2). With motion vectors (every
   default antialiasing) the lit forward shader takes 77–89 VGPRs instead of 64 and the
   vertex stage 78 instead of 49: Sponza's forward +22–27 % on the Radeon, +5–11 % on the
   RTX. Write them in the depth prepass (always drawn with the default AO), or reconstruct
   camera motion from depth and draw object motion for movers only; extend the register
   rule to the default variants (`scene_bench --pipeline-stats` prints them now).~~ Done
   2026-10-10 (ADR 0043's addendum): the prepass writes them when it runs; the default
   path's shading back at a 49-VGPR vertex stage (was 78) and 48/63/72/80 pixel VGPRs;
   Sponza 1080p TAA + AO: Radeon prepass + forward 16.64 → 15.70 ms, RTX 391 → 387 µs;
   golden images byte-identical; 16 entry points and ~40 pipelines fewer; the perf gate
   measures the default settings (`sponza_taa_ao`). Left: still geometry's motion from
   depth (the prepass now pays +0.5 ms on the Radeon for writing it).
0e. ~~**The present path on the iGPU** (review §3.4): the tone mapper writing the swapchain
   image costs the Radeon 1.1 ms of frame against writing an offscreen image and copying
   it. Offscreen + copy where measured faster, find the cause, a windowed scene in the gate.~~
   Done 2026-10-10 (ADR 0007's addendum). The cause: ref-1's monitor is on the RTX, so the
   Radeon presents across adapters (shared, row-major swapchain images). The window's
   monitor adapter (D3DKMT) is compared with Vulkan's `deviceLUID`; across adapters the
   frame is drawn apart and copied (`engine_render::present::PresentTarget`): Radeon GPU
   frame 27.1 → 26.3 ms in `flight`, the RTX unchanged (direct). Left: a windowed present
   scene in the perf gate, with the player crate (8a).
0f. ~~**Moving instances** (review §3.5): ~70 ns each per frame on one thread (60,000 make
   the RTX CPU-bound at 5.2 ms). Propagation, extraction, interpolation and upload in
   parallel on the job system; interpolation on the GPU so movers upload per tick, not per
   frame.~~ Done 2026-10-10 (ADR 0064's addendum). A mover's transform goes up once per tick
   and a compute pass places it every frame; propagation, extraction and the scene's apply
   run on the job system with the same results as on one thread (`Runtime::set_jobs`,
   `GpuScene::set_jobs`). `flight`, 60,000 movers: RTX CPU frame 4.94 / 6.99 ms → 1.53 /
   2.70 (median / p99, 60 fps), worst frame at 240 fps 1.95× the median (main 1.93×);
   Radeon CPU frame 6.43 → 2.83 ms, its GPU-bound frame 1 % longer (placement 0.45–0.53
   ms against the copy's 0.15). Found and fixed on the way: an instance that changed
   batch between ticks kept its old batch word on the GPU, and culling dropped it or
   another batch's instance.
0f2. ~~**Aligned pointer accesses in shaders** (owner, 2026-10-10: a serious topic): loads
   and stores through buffer device addresses are declared 4-byte aligned (Slang's default
   for pointers), so a driver may split 16-byte accesses; every vector load of the forward,
   culling and shadow shaders is (vertexMain 19, depthVertex 7, shadowVertex 9, cullMain 21
   and 5 stores, fragmentMain 33, the scene's scatter). Declare 16 bytes
   (`loadAligned<16>`) only where the data is so aligned (instance rows, batches; not packed
   `float3` vertices), and measure each pass on both GPUs with the register rule; golden
   images must stay byte-identical. The scene's motion passes already do (no change on the
   Radeon; on the RTX it came with their new layout, not measured alone).~~ Done 2026-10-10
   (ADR 0013's addendum): every vector access through a buffer address in the engine's
   shaders declares its alignment. RTX: forward 336 → 321 µs, the atmosphere pass 56 → 44,
   the reflections composite 128 → 118, the SSR history 61 → 57, the GPU frame of Sponza
   with atmosphere, probes and SSR 1,478 → 1,445; Radeon: the SSR depth pyramid 413 → 308,
   registers unchanged. A 4-byte value read by a dynamic index stays scalar (as a 16-byte
   load it made the SSR trace 7.5 % slower).
0g. ~~Start-up overlap (device creation beside the pack's reading and cooking) and pipeline
   variants created on first use (review §3.8): first frame 450–590 ms warm, 1.2–1.4 s
   with the pipeline cache missing.~~ Done 2026-10-10 (ADR 0079): `flight`'s first frame
   with the cache 324 → 261 ms (RTX), 433 → 367 (Radeon); with the drivers cold 796 → 504,
   1,028 → 547. The forward renderer describes its shading variants (`LazyPipeline`) and
   makes them on first use or on the device's warm-up thread; the device is created
   beside the window and the Fox's cook, the swapchain beside the renderers and the
   scene.
1. ~~GPU particles and GPU radix sort.~~ Done 2026-10-11: the sort (ADR 0080: 1 M keys
   with a payload in 0.59 ms on the RTX, 9.9 ms on the Radeon) and the particles (ADR
   0081: a GPU pool with GPU-only counts, deterministic, sorted by depth, drawn soft; 16 K
   sparks in 0.13 / 0.65 ms, a golden shot on both GPUs).
2. ~~Decals.~~ Done 2026-10-11 (ADR 0082): **a decal buffer before lighting** (the owner:
   without it the corners would come out wrong): a compute pass after the prepass over
   the tiles some decal reaches (tile and z-bin bitmasks, the wave walking its pixels'
   decals) writes albedo and a normal change with roughness and metalness; 16 shading
   entry points blend them. Golden scene's five decals: 34 µs / 0.63 ms (RTX / Radeon);
   a decal covering the screen 0.15 / 4.1 ms. Clustered decals in the shading, built on a
   branch to compare: cheaper on the RTX, worse on the iGPU (+6.4 against +4.9 ms on that
   decal). Left: MSAA views, probe captures, emissive decals, transparent surfaces.
2b. ~~**HDRI environments**~~ (owner, 2026-10-10: before GI, a necessity). ~~Phase 1~~ done
   2026-10-11 (ADR 0083): our own `.hdr` decoder, RGBE kept and decoded on the GPU (an
   unclipped sun exceeds half floats); a background cube of its own made on the GPU (a
   quarter of the panorama's width, at most 1,024 by default: 0.23 ms on the RTX, 6.8 ms on
   the Radeon for 512², once); the sun found in the panorama and taken out into the
   directional light (shadows; the 128² lighting cube without its spike, so prefiltering
   stays clean and the harmonics do not ring); turning; a golden shot with a CC0 Poly Haven
   panorama on both GPUs. ~~Phase 2~~ done 2026-10-11 (ADR 0083's amendment): cooking into
   BC6H cubes with mips (DDC; packs of their own, version 4; the background 8 times
   smaller, 2 frames of upload through the ring), the cube turned when sampled (turning
   rebuilds only the lighting: 0.14 ms for its fill on the Radeon); OpenEXR through the
   `exr` crate (1.74.0: every compression but the lossy DWA; Poly Haven's 1K EXR, PIZ
   and float, decodes to exactly its `.hdr`'s pixels).
   **Left** (§6): the sun's angular size for soft shadows.
3. ~~GI: probe volumes, then DDGI-like.~~ **Done 2026-10-11** (Echo GI; ADRs 0084,
   0087–0090), but for the leftovers at the end. Acceptance check, besides the usual: Sponza's metallic
   medallions on the curtains (roughness 0.28, metallic 0.74) must read gold when seen from
   above in shade. Today they look dark brown there. The material and the shading are
   right (seen from below they are gold), but the shaded floor they reflect has no bounced
   light (2026-10-02). Researched 2026-10-11 (ADR 0084): **our own probe GI traced with ray
   queries** (the owner, 2026-10-11: if it can be faster and cheaper than AMD's, do it),
   held to a bar: less GPU time and memory than AMD's Brixelizer GI (built from the
   FidelityFX SDK as the reference, and the fallback) at comparable quality on both GPUs.
   **Acceleration structures done 2026-10-11** (ADR 0087, `engine_render::rays`): a
   bottom level per mesh (fast trace, compacted: Sponza 7.5 / 15.8 MiB on the RTX /
   Radeon, 1.4 / 2.9 MiB at the coarsest level), a top level of the GPU scene's slots
   written on the GPU and rebuilt in frames where something changed (Sponza 80 / 230 µs;
   10,300 instances 181 / 3,798 µs), shaders reaching it by address; a ray per pixel at
   1080p 0.9 / 36 ms. **3a done 2026-10-11** (ADR 0087's addendum, `ray_bench`): the top
   level refitted between rebuilds, rebuilt after 8 refits (10,000 instances moving:
   39 / 499 µs per frame instead of 145 / 2,621, rays within 1 %; never rebuilding slows
   rays up to 8×), and at once on added, removed, teleported or re-meshed instances
   (`SceneStats::restructured`); bottom-level builds under a per-frame budget
   (`RaySettings::for_device`: Sponza at most 0.8 ms per frame on the RTX; on the iGPU
   its largest mesh alone costs 11 ms, so rays there use the coarsest level); the async
   compute queue not worth it after the refit (at most 40 µs to hide). **Phase 1 done
   2026-10-11** (ADR 0088, `engine_render::gi`): a volume of probes (octahedral
   irradiance and distance moments, relocated and classified), converged at load
   through 11 updates (Sponza's 8,680 probes: 15 ms on the RTX, 1.1 s on the iGPU, one
   update per frame), read by a resolve pass at a reduced resolution (2 × 2 pixels per
   sample on discrete GPUs, 4 × 4 on integrated ones) and 34 GI twins of the shading
   entry points (variants without GI keep their registers): 0.12 / 1.8 ms per frame at
   1080p; 12.8 MiB. The medallions read gold from above (golden shot
   `sponza_gi_medallions`). Known limits: normals from the depth facet low-poly curves. **Phase 2 done 2026-10-11** (ADR 0089): once converged,
   every frame traces windows of probes within a ray budget (131,072 rays on discrete
   GPUs, 8,192 on integrated ones), half in an 8 × 4 × 8 box around the camera and half
   in turn over the volume, blended with a hysteresis of 0.9. A sun turned off fades as
   0.9^frames (tested), around the camera first. Sponza: 0.2 ms a frame on the RTX, 1.0
   ms on the iGPU. **Brixelizer GI reference done 2026-10-11** (ADR 0090,
   `engine_brixelizer`, `docs/research/brixelizer-gi.md`): AMD's Brixelizer GI built from
   the FidelityFX SDK (three fixes in the vendored SDK for AMD's compiler, which built the
   distance field with holes on the Radeon), in the forward renderer as a screen GI
   (`ViewGi::Screen`), waiting for its distance field (else the sky leaks into its caches
   for hundreds of frames). Measured with `gi_bench` (Sponza 1080p): probe GI 0.33 / 2.4
   ms against 1.55 / 33 ms on the RTX / iGPU, 20 / 29 MiB against 594 / 532 MiB; before a
   sunlit wall 1.19 against 0.23 (physically about 1.14); a sun turned off halves in 15 /
   123 frames against 180 / 203. ADR 0084's bar is met; Brixelizer GI stays as the
   reference (the golden shot `sponza_brixelizer_gi`). The owner named our GI **Echo GI**
   (2026-10-11). **Left:** a refresh after a large change of light (on the iGPU a sun
   turned off leaves a third of its bounce after 400 frames: the far probes wait for their
   turn of the budget; `ProbeGi::refresh` converges in ten frames); local lights at the
   rays' hits (night scenes); the moments' blend (a third of the cost) at a lower rate.
3b. ~~**Frame generation out of the box**~~ (owner, 2026-10-11: "needed out of the box, DLSS FG
   and FSR FG; the HUD fix on by default"; right after GI, off by default). **Done
   2026-10-12 (ADR 0091)**; DLSS FG runs since the owner turned on Windows' GPU scheduling
   the same day. FSR FG measured on both GPUs:
   exact pacing, +half a real interval of latency, generated frames 3.6 times nearer the
   truth than the real frame a display holds (FLIP), the HUD and the panels sharp on
   them; the golden shot `sponza_fg`. Left for later: async compute for the
   interpolation; `VK_EXT_present_timing` pacing on NVIDIA; why the Radeon commits
   +850 MiB of driver memory with FG on (two 375 MiB mappings); DLSS FG's native optical
   flow queue; never offering FSR FG on small iGPUs (49 ms at 1080p). The requirements
   as they were set (`docs/research/frame-generation.md`, 2026-09-26) and the owner's:
   - **Both paths.** FSR 3.1 FG, built from the FidelityFX SDK's source (1.1.x has
     Vulkan frame interpolation and its swapchain), works on every GPU with nothing to
     install. DLSS FG through Streamline works on RTX GPUs with the user's DLLs
     (`sl.dlss_g`, `nvngx_dlssg`, `sl.reflex`/`sl.pcl`, `NvLowLatencyVk`; the engine ships
     none, §6). With DLSS FG, Reflex goes through Streamline (§7).
   - **Out of the box for the game developer.** The frame renderer offers it with no work
     on their side: the option exists in every game and is hidden where it cannot run.
     **Off by default** (owner, 2026-10-11): the player turns it on; latency measured.
   - **The HUD never interpolated, by default.** Many shipped FSR FG games smear their HUD
     until players install "HUD fix" mods; here nothing needs fixing. The frame renderer
     hands FG the scene before any UI (hudless color) and composites the UI on every
     presented frame, real and generated.
   - **Immersive UI too, with nothing to mark** (owner, 2026-10-11: a developer who makes
     the HUD diegetic, in the world and 3D, must not have to set anything). The engine
     knows how each piece of UI is made, so it chooses:
     - 2D canvases are composited after FG;
     - widgets placed in the world, or attached to the camera (a helmet visor, a cockpit,
       a gun's ammo counter), are drawn after FG on each presented frame with that
       frame's camera (interpolated for generated frames), depth-tested against the
       nearest real frame's depth;
     - UI baked into materials (a screen texture on a monitor) is scene content and is
       interpolated with its motion vectors.

     To research for the ADR: FSR FG calls the application for each presented frame (UI
     can be drawn there), while DLSS FG takes one UI buffer per real frame. The ADR picks
     what to do with world-space UI on DLSS FG's generated frames, with no flags for the
     developer.
   - **Present path and pacing.** A presenter interface (native, FidelityFX FG
     swapchain, Streamline). The limiter caps real frames; latency markers belong to real
     frames; reports tell real from generated; no frame is interpolated across a camera
     cut.
   - **Measured** on both GPUs: presented and real frame rates, latency (PresentMon,
     `engine_latency`), image quality against the real frames (FLIP), the HUD with 2D and
     3D UI.
3c. ~~**Custom 3D materials and shaders**~~ (review §4; after GI: owner, 2026-10-10). **Done
   2026-10-12 to 2026-10-14** (ADR 0092, accepted 2026-10-12), but for two leftovers. The
   ADR decides permutations and pipeline management for the whole renderer (§9 material
   graph and hand-written Slang).
   - Step 1: the passes through `IMaterial` (PR #179).
   - Step 2: material types in the renderer: typed materials, batches and pipelines per
     type, compiled on first sight with the glTF fallback drawing meanwhile (PR #180).
   - Step 3: the shading models lit, unlit, clear coat, cloth, hair and toon as a type's
     `model()` (PR #181; the built-in pipelines' registers unchanged).
   - Step 4 (the owner confirmed the material file and the scene's map, 2026-10-12):
     - 4a: material files and parameter layouts through Slang's reflection (PR #182);
     - 4b: cooking (the map, texture parameters, pack version 5 with typed materials and
       their SPIR-V, PR #183);
     - 4c: hot reload (types' modules and material files, PR #184).
   - Step 5, the ubershader experiment: measured and declined (iGPU forward +22–53 %, 90
     VGPRs; the fallback stays; branch `exp/ubershader`).
   - glTF's unlit, clear coat and sheen onto the models as built-in types (ADR 0092's
     addendum of 2026-10-13: `MODEL_SHEEN` for glTF's sheen layer, the coat's normal map).
   - PSO records (ADR 0092's addendum of 2026-10-14): the cook compiles only the entry
     points playtests drew (`*.pso.ron`). ClearCoatTest's type: 128 entry points in 4.0 s
     → 1 in 0.6 s, its pack 1.93 → 0.98 MB.
   - **Left (later):** the warm-up making recorded pipelines first; prediction for types no
     record names.
3d. ~~**Directional GTAO and GTSO**~~ done 2026-10-13 (ADR 0058's addendum): bent normals
   and cone specular occlusion behind `AoSettings::bent_normals` (the owner, 2026-10-13: a
   switch, on for discrete GPUs, off for integrated), 36 twin entry points made on first
   use; off, the shading is byte-identical to before.
4. Visibility buffer / meshlets. **Meshlets done 2026-10-14** (steps 1, 2 and 4; ADR 0085
   and its addendum of 2026-10-14), on by default since; step 3 is left. Write the ADR
   first, with no persistent per-triangle visibility bits (patent, ADR 0035 note).
   Researched 2026-10-11 (ADR 0085): **meshlets in the forward renderer first** (the owner,
   2026-10-11; both GPUs; the iGPU has no mesh shaders), the visibility buffer decided
   together with custom materials (3c).
   - **Step 1:** the cook makes meshlets per detail level (64 vertices, 124 triangles; none
     for skinned meshes) and reorders each level's triangles by them.
     - The tables sit at the end of the mesh entries, so the pack version stays.
     - On its own, the new order makes the Radeon's FlightHelmet shadows 13 % faster.
       Sponza and the RTX are within ±1 %. Golden images are unchanged.
   - **Step 2:** culling per meshlet after a depth prepass
     (`ForwardRenderer::set_meshlet_culling`). The pool keeps each level's meshlets after
     its indices.
     - Each meshlet's sphere, normal cone and (late phase) the depth pyramid are tested.
     - One indexed draw per visible meshlet through `vkCmdDrawIndexedIndirectCount`.
     - A group per chunk of 64 meshlets, not per instance: small scenes' culls were
       latency-bound.
   - **Step 4:** the shadow cascades culled per meshlet (each cascade's bounds, the cone
     seen from the light). Each run of consecutive drawn meshlets is one draw.
   - **On by default for both device classes** (`RenderDefaults::meshlet_culling`,
     2026-10-14; the owner left the choice to the coordinator). With each class's
     defaults, off → on:
     - Intel's Sponza (3.7 M triangles): −6.4 % on the RTX, −10.4 % on the Radeon;
     - Sponza: +1.8 % (24 µs) on the RTX, about the same on the Radeon;
     - 10,000 detailed spheres (`draw_bench`'s scene settings): −3 to −8 % on the RTX,
       −11 to −19 % on the Radeon.
   - Drawing pixel-bound instances whole was tried and dropped (ADR 0085's addendum).
   - **The draws' order measured 2026-10-15** (ADR 0085's note; branch
     `exp/meshlet-order`).
     - An exact order pays only for the light's order in the cascades: 12 % on the RTX's
       Intel Sponza at the default 1024², next to nothing elsewhere.
     - That is less than appends keeping an order would cost (two dispatches per list),
       so they are not built.
     - Revisit with finer cascades (2048²: −19 % RTX, −9 % Radeon) or virtual pages.
   - **The perf gate's class-default scenes cook with detail levels** since 2026-10-15, as
     a game's pack does (before, they drew full meshes). Intel's Sponza with the
     integrated defaults on the Radeon: 57.6 → 33.8 ms, its shadows 21.3 → 4.1 ms.
   - **Left:**
     - step 3, mesh shaders on the RTX (ADR 0085's addendum; deferred);
     - local lights' shadows culled per meshlet.
5. Virtual shadow maps. Researched 2026-10-14 (ADR 0093, accepted): scrolling cached
   cascades first (only what changed is drawn), virtual pages after; one cache, no
   static/dynamic pair merged by a mask (the owner, 2026-10-14).
   - **Step 1 done 2026-10-14:** the cascades kept across frames in a view's
     `CascadeCache`, on by default in `engine_frame`.
     - Shadows when walking: Intel's Sponza on the RTX 728 → 69 µs, on the Radeon
       18.9 → 2.0 ms. A still camera draws nothing.
   - **Step 2 done 2026-10-15:** moved casters draw only their old and new places, and a
     cascade's regions are drawn at once by meshlet (vertex shaders pick viewports).
     - Walking: Intel's Sponza RTX 69 → 43 µs, Radeon 1.98 → 0.76 ms. With 8 small
       objects moving: RTX 403 µs, Radeon 7.0 ms, against 728 µs and 19.0 ms drawn
       whole.
   - **Step 3, virtual pages for the sun**, designed 2026-10-15 (ADR 0093's step 3; the
     owner: after the player crate). The cache leaves resolution wanting: a shadow texel
     is never under 2.2 pixels with the defaults, 4–10 pixels at 2–5 m.
     - **3a done 2026-10-15:** the pages' machinery, not yet used by a frame
       (`engine_render::vsm::VirtualShadows`): a pool of physical pages, page tables that
       scroll with the camera, marking from the depth buffer, allocation and eviction on
       the GPU, the light turning, clearing. Every frame of the tests is held to a CPU
       model of the rules on both GPUs.
       - At 1080p (a ground plane, 3,185 pages): RTX 62 µs a frame (the first 121 µs),
         Radeon 1.03 ms (the first 1.88 ms).
     - **3b done 2026-10-15:** `ViewSettings::virtual_shadows` (off by default). Casters
       are culled into the dirty pages and drawn with an atomic minimum, without
       attachments; a pass after the prepass writes the sun's visibility into the mask
       the shading reads (the cascades' lookup skipped; transparent surfaces keep the
       cascades). Casters that change redraw every page a frame needs. Tests against
       rays on both GPUs, golden shots, three perf gate scenes.
       - Sharper shadows, and thin ones the cascades lose (a cornice's on the wall).
       - Intel's Sponza at 1080p, still: the pages' passes RTX 245 µs (the visibility
         164), Radeon 15.3 ms (13.5); the shading 34 µs and 1.4 ms cheaper without the
         cascades' lookup. Walking 5 cm a frame, drawing RTX 885 µs, Radeon 27.7 ms,
         against the cached cascades' 68 µs and 0.75 ms.
     - **3c, first part, done 2026-10-15** (ADR 0093):
       - draws clipped to the dirty pages under them (the dirty pages as bits a row,
         exact boxes, clip distances). Walking, Intel's Sponza: RTX 885 → 69 µs,
         Radeon 27.7 → 2.6 ms;
       - the visibility reads each depth once (a group-shared tile), its levels from
         group-shared memory, its 16 taps without a branch: Radeon 13.5 → 6.9 ms at
         1080p (102 → 33 VGPRs), RTX 163 → 152 µs. Now bound by the latency of its
         reads (page table, then pool).
     - **3c, second part, done 2026-10-15:** moved casters redraw only the pages under
       their old and new places (as the cascades' cache does; past 256 boxes, every
       needed page). Intel's Sponza: 1 mover RTX 828 → 257 µs, Radeon 28.3 → 9.8 ms;
       8 movers RTX 1,128 → 603 µs, Radeon 33.6 → 17.2 ms (`scene_bench --vsm`).
     - **Left:**
       1. transparent surfaces and subsurface transmission from the pages, so the
          cascades need not be drawn beside them (forward variants of their own, pages
          for surfaces the prepass does not see);
       2. a sun turning in steps (every turn redraws the needed pages);
       3. the default per device class: off for now; on for discrete GPUs is the
          owner's call (§0); the integrated class keeps the cascades (2.9 ms more);
       4. the visibility's latency on the Radeon (the table read before the barrier,
          gathers).
   - The instanced draws' pieces culled with the same tags (left from step 2).
Items 6–8 were one item 6, with sub-items 6b–6h, until 2026-10-10 (owner: item 6 had
grown, divide and conquer); the old labels stay in parentheses.

6. **Rendering leftovers.**
6a (was 6). Post effects: ~~LUT grading~~, ~~DOF~~, ~~motion blur only on cinematic
   views~~ (ADR 0067): all done 2026-10-15. Frame generation is its own item now (3b,
   after GI).
   - **Color grading done 2026-10-15** (ADR 0094): `ViewSettings::grading`.
     - The grading and the tone curve are one 48³ LUT, built on the GPU when either
       changes. Graded twins of the tone mapper's entry points read it once per pixel;
       the plain ones are unchanged.
     - Within 0.85 / 255 of the exact chain.
     - Sponza's tone map pass at 1080p: RTX 29–31 → 44 µs, Radeon 1.12 → 1.22 ms.
   - **Depth of field done 2026-10-15** (ADR 0095): `ViewSettings::depth_of_field`.
     - FidelityFX DOF 1.1's passes ported. Inverse depth instead of a signed circle of
       confusion (patent), the near field from a radius of one pixel, no ring merging.
     - After the upscalers.
     - At 1080p: RTX 0.12–0.26 ms; Radeon 3.8–8.2 ms (its composite and downsample
       bound by memory, its blur by arithmetic; to item 6b).
   - **Motion blur on cinematic views done 2026-10-15** (ADR 0096).
     - `ViewSettings::kind`: a gameplay view cannot ask for motion blur.
     - Guertin, McGuire and Nowrouzezahrai's filter.
     - At 1080p: RTX 0.43 ms, Radeon 15.7 ms (7.9 ms with 9 samples); since the
       gather runs at half size (2026-10-15, ADR 0096's addendum) RTX 0.23 ms, Radeon
       7.3 ms.
     - Object-only motion blur in gameplay views: not wanted (the owner, 2026-10-15).
       Motion blur stays on cinematic views only.
6b. The iGPU's expensive passes (review §3.11): depth of field 3.5–7.8 ms at 1080p
   (3.8–8.2 before 2026-10-15; ADR 0095's addendum: what bounds each pass, a quarter-size
   gather left), TAAU 4.2 ms (6.6 ms in the perf gate's `sponza_taa`; since
   2026-10-15 a group's render pixels are tone mapped once in shared memory, ADR 0043's
   addendum: Radeon −5 to −12 % in the perf gate's frames, RTX −5 to −7 %; the attempt of
   2026-10-11 below shared only the loads),
   ~~bloom 1.6 ms~~, ~~local shadows redrawn every frame when casters move~~ (a per-frame
   redraw budget; never the patented static/dynamic split; since 2026-10-15 moved casters
   redraw pieces of the views, ADR 0071's addendum: for the same work 25–69 % less). TAAU tried
   2026-10-11: its 3 × 3 color and depth through shared memory (the bloom's trick) made
   the Radeon's resolve 20 % slower and the RTX's 3 % faster; without its nine `exp`
   1 % faster; its history images moved in memory, unchanged. Neither its texture reads
   nor its placement limit it: about 800 lane-cycles per pixel, its arithmetic as a whole.
   The Radeon GPU Profiler cannot capture this iGPU (gfx1036); variants of the shader
   showed the per-tap tone mapping and YCoCg as a third of it (2026-10-15). What is left:
   the output store (13 %), the depth loads (6 %), memory in Sponza's frames. Tried 2026-10-11: bloom levels in R11G11B10F save the
   bloom passes 10 % on both GPUs, but on the Radeon the tone mapper then takes as much
   longer (frame unchanged; perf history). Done 2026-10-11: the bloom's taps through
   shared memory (the same values bit for bit): over the 18 perf scenes the RTX's bloom
   −36 % (every frame 40–50 µs shorter), the Radeon's −12 % (4 scenes slower by ~0.1 ms).
   Found on the way: the Radeon's memory-bound passes vary by up to 2× with where their
   images land in memory (`docs/perf/reference-hw.md`); a single-pass downsampler (levels
   kept in shared memory) is left for later.
   - **2026-10-15:**
     - depth of field measured pass by pass on the Radeon (ADR 0095's addendum): its
       composite's reads issued early and its blur writing only what the composite reads,
       the same image bit for bit: f/1.4 8.23 → 7.82 ms, f/64 3.84 → 3.52 ms. The blur is
       bound by arithmetic, the downsample by memory; in place is impossible with TAAU (its
       input is TAAU's history). A quarter-size gather (about −2.5 ms) is left until a game
       needs it;
     - motion blur's gather runs at half size (ADR 0096's addendum): Radeon 15.7 → 7.3 ms,
       RTX 0.43 → 0.23 ms at 1080p;
     - the materials' anisotropy measured: 16× → 4× saves the forward pass 1.5–3 % on both
       GPUs, 2× 7 %, trilinear 13–19 %. It stays 16×: the rest is a player's setting
       (later), not a default;
     - the sun's 3 × 3 PCF as 4 gathers tried: the RTX's forward 0.3–5.7 % faster in every
       scene, but the Radeon's shading registers rose (plain 47 → 54, environment 64 → 78)
       and 12 scenes ran 4–12 % slower. Not adopted.
6c. ~~**The perf gate's register check**~~ (the owner, 2026-10-15). **Done 2026-10-15
   (ADR 0062's addendum):** `perf_gate --registers`, run by `tools/check.ps1 -Perf` on
   both GPUs.
   - Every pipeline the perf scenes make (191 per vendor) is compared with
     `tests/golden/perf/<vendor>.registers.tsv`: its vector registers and spills.
   - A pipeline that uses more is a warning (`**more registers**`).

   The time gate had let 64 → 70 VGPRs through on the Radeon (ADR 0093); the PCF gathers
   of 2026-10-15 rose 64 → 78 and cost 12 scenes 4–12 %.
6d. ~~**Anisotropic filtering as a player setting**~~ (the owner, 2026-10-15). **Done
   2026-10-15 (ADR 0098):** `MaterialTable::set_texture_anisotropy` with
   `Anisotropy::{Off, X2, X4, X8, X16}`, 16× by default.
   - It lives in the material samplers, like the LOD bias: nothing in the shaders.
   - The bindless set holds up to 16 extra sampler sets, of any bias and anisotropy.
   - Measured 2026-10-15: 16× → 4× −1.5–3 % of the forward pass, 2× −7 %, off −13–19 %.
7. **The second review's core leftovers.**
7a (was 6c). Memory (review §3.9): ~~one transient heap for both frame slots where no
   transient crosses queues (66 / 41 MiB)~~ (done 2026-10-15, ADR 0011's addendum:
   the groups only the graphics queue touches share one heap, a barrier on the previous
   frame at each frame's start; the Radeon's transients 40 → 20 MiB, the RTX's 130 → 129
   as its AO runs on async compute), the global allocator decision (mimalloc,
   CLAUDE.md §5, never made), CPU memory accounting (0.9–1.1 GiB private bytes
   unexplained). The allocator measured 2026-10-11 (`bench/alloc-compare`,
   `docs/research/global-allocator.md`; ADR 0086, Proposed): mimalloc 2.3.2 3.3–4.7× the
   system heap on allocation-heavy work, 1.5–1.6× on glTF imports and pack loads, the
   frame unchanged; it holds memory in sleeping workers' heaps (341 MiB after a level
   load until they collect). **mimalloc chosen** (the owner, 2026-10-11) and integrated
   the same day: `engine_core::alloc::EngineAllocator` in the CLI, `flight` and
   `game_loop`, idle workers and `ThreadPool::release_memory` giving memory back, a
   counting layer in debug builds; `flight`'s first frame −5 ms, its CPU per frame the
   same, private memory +5–6 %. Left: Tracy memory events. The private bytes accounted for
   2026-10-11 (`flight` now prints its committed memory by kind, and the allocator its
   mapped bytes per heap): of the RTX's 936 MiB, 607 MiB are write-combined pages the
   driver mapped into the process (the engine's own mapped Vulkan memory 192 MiB of it),
   the process heap 56 MiB (Rust's allocations 37 MiB), the rest about 270 MiB (the
   driver's own and the threads'); the Radeon's 1,049 MiB alike (645 write-combined, 352
   of it the engine's). Left: host-visible pools start with blocks of 32 MiB (an eighth
   of the 256 MiB block, as VMA does) even for allocations of a few KiB, and every
   host-visible block stays mapped: the RTX's 64 MiB of system-memory blocks hold a few
   KiB at the end of the flight, the Radeon's 288 MiB hold 64 MiB. Freeing the empty
   blocks would give back only 0 and 32 MiB (tried). Done the same night: mapped system
   memory starts from blocks of 4 MiB (1/64): the RTX's system-memory blocks 64 → 20 MiB,
   its OS usage there 150 → 106 MiB; the Radeon's 288 → 272 MiB (its blocks there come
   from the load and stay partly used). Also left: what the driver's other
   write-combined pages are (about 300–400 MiB).
7b (was 6d). ~~Jolt's jobs on `engine_core::jobs` through JoltC's callback job system
   (review §3.10).~~ Done 2026-10-11: `engine_core::jobs::ForeignJobs` (ADR 0002 addendum:
   jobs from any thread through the injector, in a fixed set of records, a loom-checked
   free list) and `JoltBackend::set_jobs` (ADR 0018 addendum): the same results, as fast
   on the physics bench (piles of 1,000 and 5,000: 0.784 / 3.676 ms against Jolt's 0.798 /
   3.677 at 16 threads), faster on small steps; one set of threads. The runtime wires it
   when it owns physics (the player crate, 8a). A lost wake-up in `ForeignJobs::wait`
   (a sleeping waiter missed the last job) was found and fixed the same night.
7c (was 6e). ~~`forward.rs` (5,695 lines) split by feature (review §3.12).~~ Done
   2026-10-11 as files (code moved, nothing changed: golden images the same bytes):
   `forward/mod.rs` (the public types and the renderer's API, 2,421 lines), `declare.rs`
   (the frame's passes), `layout.rs` (batches and draw commands), `pipelines.rs` (a
   pipeline set per feature), `shadow_atlas.rs`, `msaa.rs`. Left: `declare` is still one
   1,650-line function; its passes per feature come with the next shading feature (decals
   or GI).
8. **The player and content** (owner, 2026-10-10: after the rendering queue).
8a (was 6f). ~~**A player crate**~~ (`engine_app`, review §3.7; ADR 0099, in four steps;
   **done 2026-10-15**, PRs #215–#220):
   device, swapchain, latency provider, process setup (power throttling, MMCSS, CPU sets),
   pipeline cache and the frame loop in one place; ECS components for lights, skinned
   meshes and probes; features per view instead of per renderer; `RenderDefaults` applied
   whole. The benches and the test bed move onto it.
   - ~~Step 1: window modes~~ (done 2026-10-15, ADR 0099):
     - windows fit into the work area, borderless full screen, and exclusive full screen
       (`VK_EXT_full_screen_exclusive`, made again from nothing when refused);
     - measured with PresentMon on both GPUs (the owner, 2026-10-10: `flight`'s window was
       larger than the screen; NVIDIA copied it through GDI and showed 1 frame in 25);
     - borderless stating "disallowed" is the default: NVIDIA flips it, 7.9 against 9.3 ms
       to the screen without vsync, the same with vsync.
   - ~~Step 2: the crate owns the wiring~~ (done 2026-10-15, ADR 0099):
     - `engine_app::Player` and the `Game` hooks; `flight` runs on it;
     - latency control on by default: RTX 7.90 → 5.66 ms, Radeon 69.3 → 52.3 ms to the
       screen;
     - no allocation in a frame.
   - ~~Step 3: lights and skinned meshes as components~~ (done 2026-10-15, ADR 0099):
     `PointLight`, `SpotLight`, `Sun` and `SkinnedMesh`, extracted by the runtime (a
     light keeps its slot while it lives); probes moved to step 4 with their features.
   - ~~Step 4~~ (done 2026-10-15, ADR 0099):
     - features per view (`ViewFeatures` in `ViewSettings`; a view without a feature is
       drawn as a renderer without it);
     - reflection probes and the GI volume as components (`ReflectionProbe`, `GiVolume`),
       each made with its first entity and free before; screen-space reflections a
       setting;
     - probes in a moving world captured in turn, at most every 30 frames for the
       scene's changes, and the render graph keeping recent frame shapes;
     - an offscreen player (`Window::Offscreen`, `Player::capture`); the golden test
       `sponza_by_day_on_the_player` holds the player's render of `sponza_day` to the
       bed's image, and the perf gate times it (`sponza_day_player`);
     - `tools/stress` a game on the player. `scene_bench`, `stream_bench` and the bed's
       feature shots stay below the player: they measure one subsystem each, and the
       player's own shot covers the game's path.
   - Follow-ups of the player: window events read again after latency control's wait
     (today they are read before it); reflection probes one face a frame on integrated
     GPUs (a whole capture is about 6 ms in one frame on the Radeon; to measure); HDR
     output from the player; the taskbar's full-screen marking; variable refresh; glTF
     `KHR_lights_punctual` into packs; rendering features on the camera component.
8b (was 6g). **Video playback** (owner, 2026-10-10: after the player crate; the engine plays
   no video today): a video frame as an ordinary texture (UI, a screen in the world, a
   viewport, cutscenes). To research and decide in an ADR: AV1 (royalty-free, unlike H.264 /
   H.265) decoded by the GPU through Vulkan Video (`VK_KHR_video_decode_av1`: check both
   reference GPUs' drivers) with dav1d (BSD-2-Clause) on worker threads where it is missing;
   WebM / Matroska demuxed by our own code; Opus sound through the existing decoder,
   synchronized to the audio clock; cost per frame measured on both GPUs.
   - **Researched and decided 2026-10-15** (ADR 0100, `docs/research/video-playback.md`).
     - Both reference GPUs decode AV1 (8 and 10 bits, film grain) on a queue of their own:
       the RTX up to level 7.3 and 8192², reference and output coinciding; the Radeon up
       to level 6.1 and 8192 × 4352, reference and output distinct.
     - A new crate `engine_video`: our own WebM reader and AV1 header parser, a Vulkan
       Video session on an optional decode queue, rav1d (BSD-2-Clause, Rust port of dav1d)
       where that is missing and as the tests' bit-exact reference, a compute pass from
       YUV to an sRGB texture, the Opus sound's clock, `VideoPlayer`.
   - **Steps:** 8b-1 the reader, the parser, rav1d; 8b-2 the Vulkan Video session,
     bit-exact against rav1d on both GPUs; 8b-3 the texture, sound and clock, the player's
     video entity, golden shots, costs, docs.
   - **8b-1 done 2026-10-15** (PR #227; ADR 0100, "8b-1 as built"):
     - `engine_video`: the WebM reader; the AV1 sequence and frame headers with the
       reference state; `Parser`, which turns a temporal unit into frames with their tiles'
       offsets and sizes (Vulkan's form); `SoftwareDecoder` over rav1d.
     - Tested against rav1d on nine generated clips (SVT-AV1, libaom, rav1e): every
       header field of every frame, hidden ones included; the test checks the clips
       still use each of the features it lists.
     - Measured on ref-1: the parser costs 0.3 µs a unit. rav1d decodes a 13 Mbit/s
       1080p30 stream at 6.5× real time on one thread (15× with frame threads), a
       36 Mbit/s 2160p30 10-bit one at 1.7× (5.5×).
     - New build requirement: NASM on `PATH`.
   - **8b-2 done 2026-10-16** (PR #228; ADR 0100, "8b-2 as built"):
     - `engine_rhi`: the AV1 decode queue, created when asked for
       (`ExtraQueues::video_decode`: 2.5 ms of the RTX's device creation, nothing on the
       Radeon).
     - `engine_video::gpu::GpuDecoder`: the session, nine reference slots, output images
       shared with the graphics queue. The RTX decodes into the reference and copies on its
       decode queue; the Radeon decodes into the output.
     - Every picture of the clips each GPU takes is bit-exact against rav1d (film grain
       too), under validation: all 11 clips on the Radeon, 8 on the RTX (it takes nothing
       below 128 × 128).
     - Two driver facts worked around:
       - AMD reads the tiles' starts in superblocks;
       - SVT-AV1 1.8 writes frames taller than the sequence header allows, which AMD's
         decoder clips.
     - Measured: 1080p30 at 34× real time on the RTX, 18× on the Radeon (rav1d on 16
       threads: 15×); 2160p30 10-bit at 9.2× and 3.6× (rav1d: 5.5×). 20–60 µs of CPU a
       unit.
   - **8b-3 done 2026-10-16** (PR #229; ADR 0100, "8b-3 as built"):
     - The sound: the Opus track as an `engine_audio` stream, exact seeks; its position in
       the mix less the output's latency is the clock.
     - The picture: a compute pass in the frame graph converts each new picture into the
       video's RGBA8 texture (sampled as sRGB): the stream's matrix, range, depth and
       siting; GPU pictures read where they were decoded. Within one per channel of a CPU
       reference on both GPUs.
     - `VideoPlayer`: decoding ahead, the clock, play, pause, seek, loop, speed, events;
       rav1d takes over where the GPU refuses a frame on the way.
     - The player: `Videos` opens videos from bytes and shows them in materials (external
       pool textures) and UI sprites; `PlayerSettings::video_decode`; a golden shot per
       vendor.
     - Measured: a 1080p picture's conversion 25 µs on the RTX, 1.3 ms on the Radeon iGPU
       (2160p 10-bit: 0.17 and 4.3 ms).
     - Left (§6, Video): packs, HDR, transparency, spatial sound, seamless loops.
8c (was 6h). **Colliders made automatically**: collision shapes from meshes at import and
   cook, into the pack ("one button"): static level geometry as triangle meshes (simplified
   where it pays), props as a fitted box, sphere or capsule or a convex hull, concave ones
   split into convex parts (CoACD, MIT, or V-HACD, BSD-3: compare), and colliders authored
   in the source taken as they are (`UCX_` names, a glTF physics extension). Today shapes
   are only made in code.
   - **Research and ADR 0101** (2026-10-16):
     - V-HACD 4.1 against CoACD 1.0.14, judged exactly with manifold booleans: CoACD fits
       better with 2-10 times fewer hulls (sticking out 0.05-1.6 % of the size against
       1-9 %), and is 20-90 times slower;
     - Jolt's mesh shape costs about 1 µs a triangle to build, and restoring its binary
       form is 470-880 times faster (Intel Sponza 3.9 s → 5.5 ms).
   - **Owner's requirements** (2026-10-16):
     - a decomposition of our own in Rust;
     - any scale after the colliders are made, with no errors;
     - colliders as editable data for the editor later.
   - **Done (first PR, #230):**
     - 8c-1: `engine_collision`: QuickHull, fitting, scale rules;
     - 8c-2a: glTF physics, the cook's colliders, pack version 6, baking in `ofeliaengine
       cook`;
     - 8c-4a: `ScenePhysics` and `Player::load_physics`.
     - Sponza's colliders cost the cook 0.33 s and its pack 2.4 MB; Intel Sponza's, 1.8 s
       and 37 MB.
   - **Done (second PR, #231, 8c-3):** `engine_collision::decompose`, CoACD's algorithm in Rust
     without its dependencies (our marching-tetrahedra remesh, quadric decimation, ear-clipped
     caps, R2 samples); parallel where it changes nothing; in the cook for props
     (`DynamicShape::Auto` and `Parts`) and dynamic glTF bodies' mesh colliders, cached in
     the DDC. 14 models in 0.02-3.0 s (CoACD 1.4-195 s) with CoACD's hull counts but the
     mug's (24 against 21); sticking out within 0.1 % of CoACD on 7, up to 1.1 % more on
     smooth ones (64 points a hull).
   - **Done (third PR, #232, 8c-4):** `concave_props_scaled_every_way_land_on_sponza` (nine
     props at scale 1, 0.5, 2, along their axes, mirrored, sheared; at rest standing on bare
     floor found by rays; a ball into the U's notch reaches the floor) and the rendered demo
     `engine_app/examples/props_drop.rs` (21 props, 5 meshes decomposed in 0.53 s). A mesh
     of several closed pieces is now remeshed: as it was, it ran through itself.
   - **Left:**
     - the rules file (the owner's question 1), with a default part limit for props;
     - hulls bounded by an error instead of 64 points;
     - `RayHit`'s surface;
     - the cook's colliders on all threads;
     - a sheared prop's drawn mesh (the ECS transform has no shear; the collider has it).
8d. **Localization and internationalization for the game developer** (owner, 2026-10-14:
   simple to use). Today the engine has none. To research first (Fluent and its Rust crates,
   ICU4X, gettext's PO format, Godot's `TranslationServer`, Unreal's string tables and text
   gathering, Unity's Localization package) and decide in an ADR:
   - text string tables per language as source assets (merge-friendly, stable keys), cooked
     into the pack;
   - a lookup from scripts and Rust with arguments, CLDR plural and gender rules, numbers and
     dates per locale;
   - the language switched at run time, the tables hot-reloaded;
   - per-language fonts and fallbacks, right-to-left and complex scripts through
     `engine_text`'s shaping and bidi;
   - a check for missing and unused keys ("one button": the CLI and later the editor).

   **Done 2026-10-16 (ADR 0102, Proposed; PRs #233 and #234):**
   - **8d-1:** the research note `docs/research/localization.md` and the ADR.
   - **8d-2:** `engine_loc`. Fluent's syntax compiled into tables, formatted into the
     caller's string without allocating (14-148 ns a message), plurals, numbers and
     locale fallback by ICU4X 2.3, pseudo-locales.
   - **8d-3:** pack entries `Strings` (version 7); `ofeliaengine loc build` and `loc
     check` (what each locale lacks, translations' variables, keys the code uses but
     nothing defines, keys nothing uses). The player's text: a pack, or a `loc/` folder
     compiled again on its own thread when a file changes (an edit showed 3 frames later);
     the player's languages or Windows'. The dev guide `docs/dev-guide/localization.md`.
   - **8d-4:** UAX #9 in `engine_text`. It is adapted from unicode-bidi (7 to 23 allocations
     a call there, none here; 659 ns for a 48-character Arabic line). All 91,707
     conformance cases pass. Lines are reordered and aligned by their paragraph's
     direction, and brackets mirror. `FontStacks` per language. Noto Sans Arabic and Hebrew
     are bundled for tests. A golden image of text in both directions.
   - **Left:** the owner's question on the format; a UI mirrored for right-to-left
     languages (item 9); dates.
8e. ~~**Color vision settings for the game developer**~~ (owner, 2026-10-14: a light
   addition). **Done 2026-10-15 (ADR 0097):** `ViewSettings::color_vision`.
   - Protan, deutan and tritan, corrected (daltonization) or simulated, with a severity.
   - Held by the graded tone mapper's LUT after the curve: nothing while off, the graded
     path's cost while on.
   - Golden image `sponza_deuteranopia`.

   The item as it was asked:
   - protanopia, deuteranopia and tritanopia for the player: their correction (daltonizing)
     with a strength;
   - their simulation for the developer, to check what such players see;
   - a 3 × 3 matrix in the tone mapper, at no cost while off (its own entry point or a
     specialization constant).

   To research first: Machado et al. 2009, Brettel, Viénot and Mollon 1997, daltonization,
   Unreal's `r.Color.Deficiency`, what shipped games offer.
9. Then: editor tools, Luau bindings and debugger, other tooling.
10. **After the base** (team decision, 2026-10-20: quality of life for now). They come
    after the queue above: the editor's phases, Luau and the layer for Claude Code. This
    replaces 2026-10-16's "terrain, foliage, water and impostors stay in §6 until a game
    asks". Each starts with its research note:
    - **terrain:** height fields with levels of detail, layered materials, Jolt's height
      field colliders and the navigation mesh made from them, large worlds streamed;
      sculpting and painting in the editor;
    - **water:** rivers, lakes and the sea, their shading, and buoyancy;
    - **foliage:** grass and trees scattered over a surface, instanced and culled on the
      GPU;
    - **HLOD and impostors** for distant objects in open worlds;
    - **VST3 hosting** (CLAUDE.md §11): scanning in a separate process, isolation, delay
      compensation, the plugins' state and windows, an effect baked in the editor.

## 6. Known gaps and leftovers (candidates for the queue)

- **Sounds spawned to end on their own never end without an audio device** (found
  2026-10-30 by E12-3b-1's test): an `AudioSource3D` despawned at its end (a clip event's
  sound, a script's `audio.play_at`) stays in the world while no device plays it, so a
  dedicated server or a headless run gathers one entity per sound. The sound system
  should end them by their length when no voice plays them.

- **Clip events as hooks of their names** (team decision, 2026-10-30, ADR 0124's
  E12-3b-1): `on_animation_event(name, data)` only for now; an event calling the function
  of its name as `send` does may be added beside it later (about an hour).

- **A scene with no model draws the clear colour, not its sky** (found 2026-10-29 by
  E12-1a's test, not looked into): with its only model removed, CesiumMan's test scene
  drew a uniform dark blue; with a floor beside him, the sky drew. Something skips the
  view when no instance is drawn.

- **Fixed (2026-10-29): a material type's reload compiled two or three times** (found
  with E10-3f-2, 2026-10-28; #446): the entry points, the node previews' apart with their
  define, and the parameters' layout in a compile of its own. Now one Slang request does
  all three (the editing build's module defines `MATERIAL_NODE_PREVIEWS` itself), and a
  graph's code edit reaches the sphere in 0.39–0.48 s on both GPUs (0.62–0.77 s before),
  within the 500 ms budget. Left: the editor's own writes still wait out the 30 ms of
  quiet kept for other editors' saves.

- **The colliders' overlay in heavy scenes** (ADR 0117, 2026-10-22): with 10,000 models
  shown it checks every model's place each frame (99 µs of the editor's frame) and copies
  its 120,000 lines into the frame's ring (2.9 MB: 0.26 ms of the RTX's frame, over
  PCIe). Change detection, as `ScenePhysics::sync_models` has, and the lines kept on the
  GPU while they do not change would take both away. Only while the overlay is shown.

- **The code editor's leftovers** (E8-1, 2026-10-22, ADR 0107's as-built):
  - the scene is still drawn under a script's code (its GPU time spent while coding),
    until the split view of code beside the scene skips its frame when no view shows it;
  - a file changed outside with unsaved changes is told, not compared;
  - closed scripts are not in "Reopen Closed";
  - IME composition is not shown in place;
  - wide (East Asian) characters take one column of the grid but two on screen.

- **FIFO did not pace on ref-1** (2026-10-22, ADR 0023's addendum): the flight with
  `--vsync` in a window drew 570 frames a second (500 with Reflex). On 2026-10-12 and 15
  it paced at 240 Hz. Not looked into: a driver setting forcing vsync off would do it;
  PresentMon would tell.

- **Prefabs** (E5, 2026-10-22):
  - undoing a following puts the old prefab's values back while the file is new: saving
    then writes them as overrides (undo it only to step past it);
  - Revert does not bring back the entities and components an instance removed;
  - the player finds prefabs by the path written in the scene only (the editor by id,
    through the asset database);
  - undo does not keep what an instance could not place (`KeptBlocks`).

- **Fixed (2026-10-23): the editor's frame allocated** (found 2026-10-22, E4-2; ADR 0107
  §8 asks for 0 per frame in steady state, panels included).
  - `engine_editor`'s `editor_frame_zero_alloc` measures the whole editor over the player:
    - a tree of 25 named entities, one selected in the inspector;
    - files in the dock;
    - the pointer moving over the viewport and the panels, so every frame is built.
  - It went from 23 allocations a frame (10 with the inspector's part alone) to 0. The
    sources were:
    - the shell's lists built anew each frame (tree, folders, files, tabs, menus, the
      inspector's values, fields and sections);
    - `roots()` and the tree's walk;
    - three paths joined in `tick_assets`;
    - a query made in `ensure_prefab_bases`;
    - the outline's walk;
    - the inspector's component list and field paths;
    - a section's title put in lower case with no filter.
  - The fix keeps the lists' room between frames (`recycle`: an in-place `collect` moves
    a list's allocation to values of another lifetime). The test prints the stacks of the
    first allocations when it fails.
  - Not covered yet: a code tab, a multiple selection, Play.

- **The Radeon's CPU check runs 70–85 % above its baseline** (2026-10-22, from #307 on):
  `perf_gate --cpu`'s cpu frame 374 → 640–693 µs, every stage alike (`simulate` and
  `pools` too), on main and the branches the same (A/B with prebuilt binaries: main
  649/644, branch 644/639), while NVIDIA's CPU check stays within 1 %: the machine's
  state, not code. Team decision, 2026-10-22: `tests/golden/perf/amd.cpu.tsv` is not
  refreshed for now; its flags are known noise while NVIDIA's CPU check is clean.
  Refresh it if the shift stays once the machine is quiet.

- **Fixed (2026-10-23): `zero_alloc_probes` failed under the gate's load now and then**
  (6 allocations after 160–235 warm-up frames, the GI volume converged; alone 0). Found
  with stacks the zero-allocation tests now print for their first allocations:
  - the render graph's cache drops the images no frame used for 128 frames (the probe's
    capture targets, after its one capture);
  - their bindless indices went to release lists that started empty, so the first
    releases of a run grew them (0 → 4 → 8 → 16, in both lists: the 6);
  - under load the warm-up ended before the drop.

  The lists keep room for 256 indices now (`engine_rhi` `RELEASE_ROOM`). Six copies at
  once: 2 of 18 runs allocated before, 0 of 24 after.

- **Rendering:**
  - **the meshlet draws' order follows the GPU's scheduling** (atomic appends). A meshlet
    cull with fewer registers on the RTX (47 → 40) ran more groups at once and shuffled
    the draws. That cost the passes drawing them their front-to-back order: Sponza,
    prepass +8 %, shadows +9–13 % (ADR 0093). Appends that keep the work items' order
    (counts, a scan, then writes) would make it robust.
    - Measured 2026-10-15: the instances' own order, kept exactly, is no better than the
      GPU's. Only the light's order gains, in the cascades (ADR 0085's note).
    - Not built: at the default 1024² it saves less than the appends would cost;
  - **cached cascades** (ADR 0093): instanced draws (meshes without meshlets, material
    types) still go piece by piece, each drawing every caster the cascade's regions reach;
    a rendering per layer costs the Radeon 6 % when everything is drawn whole (the first
    frame, a cut, the light turning); the moved casters' clusters are greedy;
  - **the perf gate let a register step through:** ADR 0093's step 1 took the Radeon's
    environment shading from 64 to 70 VGPRs (forward +5 % on Intel's Sponza) under the
    AMD threshold (10 %, 2 % of the frame); step 2 brought it back. A check of every
    pipeline's registers against a per-vendor baseline (the drivers' statistics, which
    do not vary from run to run) would catch such steps at once;
  - half-resolution aerial perspective and fog on the iGPU (about 1–2 ms there);
  - froxel volumetric fog;
  - per-pixel local-shadow mask (the shadowed forward shading costs +10–12 % on the iGPU);
  - softer shadow filters; an HDR panorama's sun is a point light today: its angular size
    (ADR 0083's sun search can measure it) for soft shadows;
  - contact shadows of local lights (owner, 2026-10-10: on request): Bend's pass per light,
    for the brightest one or two; today only the sun has them;
  - reflections on transparent surfaces;
  - probe shadow-map caching;
  - XeSS;
  - Brixelizer GI (the reference, ADR 0090): its specular is not composited, and
    reflection probes captured in its views see only the sky. The SDK's problems were
    reported to AMD on 2026-10-11 (the owner: for other developers), FidelityFX SDK
    issues #202–#204; AMD dropped Brixelizer from the SDK's 2.x line, so no fix is
    expected. The driver bug under the holes (NoContraction lost when an undecorated twin
    exists; `engine_rhi`'s `contraction_repro` example) goes through AMD's Bug Report
    Tool, by the owner; re-run the example after AMD driver updates;
  - ambient light inconsistent indoors without GI: diffuse ambient comes from the sky's
    irradiance (barely occluded), specular from reflection probes that see the real,
    darker interior. With probe GI (ADR 0088) they agree; on GPUs without ray queries
    irradiance from the reflection probes (SH of their captures) would still help;
  - anisotropic GGX is missing from the material model (cloth, clear coat and hair are
    shading models since ADR 0092);
  - FSR 4: ML, RDNA4-only, shipped as a DLL (DX12 at first). Check Vulkan availability and
    license; it needs an RDNA4 GPU to test;
  - `ERROR_DEVICE_LOST` on the Radeon, in the process or as a crash of it, when an MSAA
    view with texture streaming draws beside another device in one process (found
    2026-10-13, on main too; narrowed 2026-10-14):
    - fails on every run, with a lost device or an access violation of the process
      (`STATUS_ACCESS_VIOLATION`, 2 runs of 3): Sponza with MSAA 4× and a texture budget
      (streaming by feedback, ADR 0076) as one thread and any other golden shot as the
      second (`texture_feedback_with_msaa flight_helmet --test-threads 2`);
    - passes: Sponza with MSAA 4× or 2× without streaming, with SMAA, streaming without
      MSAA; a bare MSAA pass with a resolve on two devices in one process in every mix
      (`engine_rhi`'s `multi_device_msaa` example: not the driver alone); one device;
      separate processes (the gate); the RTX in any mix;
    - validation off: still fails. GPU-assisted validation on: it stops failing (timing or
      memory placement). No TDR (no event 4101), and the fault probe finds no device lost.
    - **Narrowed further (2026-10-14, the `fix/radeon-device-lost` branch's repro):**
      - The trigger is another instance of the process being destroyed
        (`vkDestroyInstance`) while the device renders. The device is lost at its next
        submit.
      - A second instance with an idle device is enough: destroyed while Sponza renders,
        the loss follows; destroyed after, it does not (4/4).
      - Its device destroyed while Sponza renders and the instance only after: no loss
        (3/3).
      - The overlays are not the cause. RivaTuner's and OBS's implicit layers were in
        every process: an elevated process's loader ignores `VK_LOADER_LAYERS_DISABLE`.
        Turned off by their own switches, the loss stays.
      - So most likely the driver's (26.8.1): independent instances should not affect each
        other.
    - **Worked around:** `engine_rhi` destroys an instance that goes while others live
      only with the last of them. Checked on the Radeon after a restart:
      - the repro passes 8 rounds of 8, with an idle second device and with FlightHelmet
        beside;
      - `texture_feedback_with_msaa flight_helmet --test-threads 2` passes, where it failed
        every time.
    - The older driver 32.0.21036.18 (November 2025) has the same bug: not a regression of
      26.8.1.
    - Some fifteen losses in a row stopped the Radeon's driver (code 43; back after
      `pnputil /restart-device` and a restart).
    - Found on the way and fixed: the derived data cache's temporary file was named by the
      process only, so two threads caching one texture at once failed (now one per write);
  - glTF's texture coordinates beyond the first set, `KHR_texture_transform` and vertex
    colors (Khronos' SheenChair needs the first two);
  - physical light units (the owner, 2026-10-13: for the future, much later): emission in
    nits (cd/m²) or EV, the sun in lux, local lights in lumens or candela, a camera with
    EV100, and pre-exposure (100,000 lux overflows FP16) as Filament and Frostbite do.
    Today every light and emission is on one relative scale (the sun's color about 3–4,
    emission added as is, exposure fixed or from the histogram). It changes all lighting:
    an ADR first.
- **Patents avoided on purpose** (don't build without a new decision): static/dynamic
  split of cached shadows (Warner Bros US 11,908,062 / 12,315,068); dual-quaternion
  skinning variants (US9613456B2); motion matching variants; Burley's diffusion profile;
  higher-order texture filters from combined bilinear fetches (ATI US 8,675,007 B2, in
  force until 2032: Catmull-Rom in 9 or 5 fetches; read texels one by one instead).
- **Animation:** morph targets, state machines and blend trees, retargeting, SIMD clip
  decoding (wins back 2× decode speed).
- **Networking:**
  - transport: congestion control beyond the send budget, connection migration;
  - servers: a reference rendezvous/relay server binary;
  - replication: predicted spawns (projectiles), per-component update rates,
    PVS/portal relevance, built-in visual error smoothing;
  - runtime: a windowed demo (needs a dilated client clock in `engine_runtime`).
- **Streaming (ADR 0076):** memory priorities (`VK_EXT_pageable_device_local_memory`),
  feedback from several views, LOD crossfades and impostors, sparse residency. Screen-size
  prefetch is blocked: the Graphine texture-streaming prediction patents (US10748241 and
  family) claim combining prediction schemes, so feedback is the only source of wanted levels.
- **Antialiasing (ADR 0075):**
  - centroid interpolation under MSAA;
  - per-sample or bilaterally upsampled AO and contact shadows under MSAA;
  - dynamic resolution with MSAA;
  - CMAA is not built (Intel patents).
  - TAAU's optional Catmull-Rom history reads its 12 texels one by one since 2026-10-10
    (owner's decision; it merged bilinear fetches as ATI's US 8,675,007 B2 claims): RTX
    resolve 162 → 206 µs at 1080p, Radeon 4.96 → 5.28 ms; the default bilinear one is
    unchanged.
  - Ambient occlusion: the discrete default stays High (owner, 2026-10-10), although it
    is 28 % of the RTX's default GPU frame (481 µs; Medium 0.40 ms, FLIP 0.096 vs 0.070).
  - The discrete-GPU default stays TAA. The other candidate is MSAA 4× (+1.2 ms on the
    RTX, sharpest in motion, more flicker); the owner decides.
  - A Streamline `eErrorExceptionHandler` hang was seen once in `aa_bench`.
  - ~~`procedural_contact_shadows` renders a few pixels differently from run to run on
    both vendors~~ (5–11 pixels, up to 25 levels): Bend's layout writes many pixels more
    than once, and the last write won at depth edges. Made deterministic 2026-10-11 (the
    owner: try it; ADR 0071 addendum): an R32 mask with an atomic minimum. Cost: the
    RTX's contact passes +12–28 µs, the Radeon's +63 µs and its forward pass +180 µs
    (about 2 % of its frame), only in views with contact shadows. `procedural_local_shadows` on the Radeon differed from its golden image
    (97 pixels, a short streak on a shadow edge) because that image was written on
    2026-10-01, before the iGPU's SoC voltage fix; written again (renders are the same
    bytes run after run on both GPUs, checked with exact test names). `sponza`'s
    reported difference came from an old render: the current one matches.
- **Runtime:**
  - transform propagation of non-root candidates (children whose own transforms change
    every tick) stays on one thread, by depth;
  - the GPU placement of moving instances costs the Radeon 0.45–0.53 ms per frame at
    60,000 against the old copy's 0.15: whole 64-byte instance writes (the material and
    batch word kept beside the motion planes by every writer of that word) measured 85 µs
    less.
- **Presentation:** test 0e's choice with a monitor on the iGPU (the owner can connect one;
  later, owner 2026-10-10).
  - **Exclusive full screen hung once** in a session that could not bring its window to
    the front (2026-10-16): the flight drew nothing for 300 s.
    - Not reproduced in three runs the same day with the owner's monitor. Each time the
      window was focused at once. Without vsync NVIDIA refused the monitor and the flight
      ran borderless; with vsync it held the monitor (4.15 ms at 240 Hz).
    - Since then (#240) the flight stops after 10 s without a frame and prints the
      player's presentation, and the player logs every swapchain it makes anew
      (`engine_app::present`).
    - If it comes back: the player should give up on the monitor after a loss or two and
      stay borderless until the window is focused.
- **Latency (ADR 0023's addendum):** a pacer of the engine's own for frames shorter than
  the driver's floor (Reflex never sleeps under ~1.5 ms: above ~500 FPS frames hold at
  2 ms). It would sleep on the precise timer until the GPU is predicted free, from the
  engine's own GPU times. The latency overlay should read input to GPU done from the
  frames' timeline, since Reflex's reports are a frame late with tagged submissions.
- **Video (ADR 0100):**
  - **transparency** (the owner, 2026-10-15: later): an alpha channel, for instance a
    second, monochrome AV1 stream decoded beside the picture;
  - WebM files cooked into packs and streamed through the pack's reads (videos open from
    bytes today);
  - HDR (PQ, HLG) and BT.2020's primaries: an RGBA16F texture and the curve;
  - spatial sound for a video in the world; a seamless loop (the next round's first
    picture decoded before the end);
  - the conversion on the Radeon iGPU: 1.3 ms a 1080p picture (stores and fetches about
    equal); a fragment shader writing through the render backend might halve it;
  - `Game::ui` is never called by the player (its UI goes through `Player::ui_mut`): call
    it or remove it;
  - streams no test clip has: intra-only and switch frames, short reference signaling
    (unit-tested only), tile sizes that are not uniform, loop filter delta updates, film
    grain loaded from a reference;
  - the GPU decoder (8b-2):
    - decode status queries, so a damaged stream is reported, not just decoded wrong;
    - one decode queue shared by several players;
    - videos below 128 × 128 on the RTX go to rav1d.
- **Particles (ADR 0081):** lighting and shadows on them, depth-buffer collisions, trails
  and mesh particles, user simulation code, large particles drawn at a lower resolution
  (overdraw on the iGPU: 256 smoke puffs cost 1.6 ms there), a one-workgroup sort for small
  counts (the sort is most of 16 K particles' cost), ECS components, motion vectors for
  particles, orthographic views.
- **Start-up (ADR 0079):**
  - acquiring the swapchain image late would let the Radeon's first frame record beside its
    swapchain (it waits 140 ms for it now);
  - `VK_EXT_shader_module_identifier` lookups: a large shader's lookup takes 1.1 ms on
    NVIDIA;
  - a record of the variants a game draws with, so the warm-up makes them first (with the
    material graph);
  - the other renderers' pipelines (TAA and upscalers, post, AO's and local shadows'
    passes, skinning) are still made at start-up.
- **Reliability:** two silent process exits (`perf_gate` once, `frame_bench` once) were
  never reproduced or explained. Watch for a recurrence and get a stack.
- **Housekeeping:** `scene_bench` is wired by hand, outside `engine_frame`. From the second review:
  - ~~`game_loop`'s "input → camera" counts a stale window event (637 ms)~~ (fixed
    2026-10-11: cursor events from before the loop are ignored);
  - ~~`scene_bench --ao` takes the next argument as the AO quality even when it is a
    flag~~ (fixed 2026-10-11);
  - CLAUDE.md §2 ("engine_runtime: game executable") describes a thing that does not
    exist yet (§5's mimalloc does since 2026-10-11).

## 7. First task of a new coordinator session: a fresh-eyes technical review

The second review was done on 2026-10-10: `docs/research/core-audit-2.md`; the third on
2026-10-16: `docs/research/core-audit-3.md`. Their harness `engine_app/examples/flight.rs`
measures a game's frame end to end; rerun it in later reviews to compare (windowed:
`--window windowed`, people work at ref-1).

**Owner's instruction (2026-10-02): do this review yourself, and start no agents** (no
`Agent` tool, no subagents, no background workers) until the owner explicitly says agents
may run again. Read, measure and write on your own; ask the owner if something needs
another pair of hands. Since 2026-10-20 agents may run again within §2's limits; the
review itself stays the coordinator's.

Before taking the next item from the queue, review the core again with fresh eyes. The
first audit (2026-10-01) produced most of the work above, so a second look now pays. Work
from the code and from measurements, not only from the docs:

1. **Read the code of the hot paths** and judge it against the best published practice:
   - frame setup and the render graph;
   - GPU scene upload and extraction;
   - culling, the forward shading and its register use;
   - the frame renderer;
   - the job system;
   - ECS iteration;
   - asset streaming;
   - networking per tick.
2. **Measure what has no number yet:**
   - CPU time per frame broken down by system, with Tracy on Sponza and on a stress scene
     with many moving and skinned objects;
   - worst-frame hitches while flying through a scene;
   - start-up time and time to first frame;
   - shader and pipeline warm-up;
   - GPU and CPU memory;
   - all of it on both GPUs.
3. **Look for structural weak spots:**
   - APIs that disagree with each other;
   - duplicated wiring (for example the test bed vs. the runtime vs. the benches);
   - `unsafe` code without a written invariant;
   - missing tests (what would break silently);
   - docs that drifted from the code;
   - features that cost something when unused.
4. **Compare with what a shipped engine needs and we lack.** Examples: shader permutation
   control, LOD and impostors, terrain, foliage, water, decals, UI rendering performance,
   localization, save games, the crash pipeline in shipped builds, platform pacing.
5. **Write the findings** to `docs/research/core-audit-2.md`, ranked by impact on the core's
   quality and speed, with a cost estimate for each. Merge them with §5 and §6 into a new
   queue, then report to the owner before starting.
