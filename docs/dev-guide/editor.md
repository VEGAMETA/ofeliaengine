# The editor

The editor runs inside your game's own build (ADR 0107): the player runs the game's world,
draws the scene into the viewport panel, and the editor's shell (the team's Godot layout)
is drawn over the frame. Your components and systems are native in it.

```text
cargo run -p engine_editor --release --bin editor -- scenes/main.scene --project C:/games/mygame
```

- **`scene`**: a scene file (ADR 0108), relative to `--project` (the working folder by
  default). Without one, an empty world.
- **`--frames N`** renders N frames and exits (smoke tests), printing the time with the
  start.

From a game's own build:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The game's settings; their `scene` is what the editor opens.
    engine_editor::run(settings()?, start)
}
```

`start` makes the game from the player when Play starts (Play comes with phase E3).

## What works now

- **The viewport** shows the scene through the game's renderer (every feature the player
  has), from the editor's own camera, in the panel's rectangle.
- **The scene tree** lists the scene's entities (those with a scene id) in their hierarchy:
  - each row shows its name (`Name`) and its kind (camera, sun, light, model);
  - a click selects a row; a click on the selected row opens or closes it;
  - **`+`** (or Ctrl+A) adds an entity where the camera looks, on the grid: an empty
    entity, a cube, sphere, plane or cylinder, a sun, a point or spot light, a camera.
- **The inspector** edits the selected entity's components, field by field:
  - **numbers** are dragged sideways (see snapping below); vectors show their parts with
    colored `x y z` labels, colors `r g b a`;
  - **a rotation** shows as Euler angles in degrees (applied Y, X, Z: yaw, pitch, roll);
  - **checkboxes** for booleans, **a list** for enums;
  - strings, references and containers show as text: typing them comes with the text
    field (phase E1-4b), asset pickers with the asset browser (E4);
  - **Escape** during a drag puts the value back.
- **Everything is undoable**, one step per action (a whole drag is one step). The scene's
  tab shows `*` while there are unsaved changes; undoing back to the saved state clears it.
- **Saving** writes the scene file under the scene's own id, asset references by path.
  It is written beside the file and moved over it, so a failed save leaves the old file
  whole. A new scene is saved as `untitled.scene` in the project folder.
- **Edit mode:** the game is not made and nothing ticks.

## Shortcuts

| Command | Keys |
|---|---|
| Undo | Ctrl+Z |
| Redo | Ctrl+Y, Ctrl+Shift+Z |
| Save the scene | Ctrl+S |
| Delete the selection (with its children) | Delete |
| Duplicate the selection (with its children) | Ctrl+D |
| Add an entity | Ctrl+A |
| Snap by default, or not | Y |

Keys are physical positions, so they stay put on any keyboard layout. Every command has a
stable name (`edit.undo`, `file.save`, `scene.duplicate`…) that the command palette, the
keymap file and scripts use; `EditorGame::run_command` runs one by name.

## Snapping (the owner's rules, 2026-10-17)

| While dragging | What the value does |
|---|---|
| nothing held | follows the pointer freely (the default) |
| Shift | the other mode for that moment: whole steps from where the drag started (0.37 → 0.47, 0.57) |
| Ctrl | rounds to the grid itself: the object's center lands on it (0.37 → 0.4, 0.5) |

- Steps: position 0.1, scale 0.1, rotation 45°, other numbers 0.1 (or their field's own
  `#[reflect(step = …)]`), whole numbers 1.
- The grid button in the tool bar (or Y) makes snapping the default; Shift then drags
  freely.
- The viewport's gizmos (phase E2) follow the same rules.

## Values from code

`EditorGame::set(world, entity, "Transform.translation", "(3, 0, 0)")` sets a value by its
scene path from scene text, as one undoable step: for tests, scripts and the command line.

## Viewport navigation

| Action | Input |
|---|---|
| Orbit around the pivot | middle drag |
| Pan | Shift + middle drag |
| Zoom toward the pivot | mouse wheel |
| Look around (the pivot follows) | right drag |

The bindings follow Godot (`docs/research/ux/viewport-navigation.md`). Flying with
right button + WASD and focusing the selection come later.

## Coming next

| Phase | What |
|---|---|
| E1-4b | text fields (rename with F2, typed values and expressions, strings), the command palette (Ctrl+Shift+P), multi-selection and multi-editing, the inspector's filter, reset to default |
| E1-5 | sessions, autosave, rebuild and restart after a code change |
| E2 | selection in the viewport, gizmos, placement by drag and drop |
| E3 | Play on a copy of the world |

The whole plan is ADR 0107 §7.

## HDR

The editor presents in SDR for now: it draws through the player, which presents in SDR.
HDR output comes back with the player's own HDR output, which games get too.

## What it costs (ref-1, release, 1600×900)

| | |
|---|---|
| Start and 60 frames | 0.51–0.69 s (budget for the start alone: 2 s) |
| The editor's frame on the CPU (shell, tree of 200 entities, inspector) | 74 µs, 81 µs with a selection |
