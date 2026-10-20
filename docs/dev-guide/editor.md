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
  - each row shows its kind's icon, its name (`Name`) and its kind (camera, sun, light,
    model); a model's icon is its shape's (cube, sphere, plane, cylinder), the same icons
    as in the add menu and the palette;
  - a click selects a row; Ctrl+click adds it to the selection or takes it out;
    Shift+click selects the rows from the last clicked one. The last picked is the
    active one: the inspector shows it, F2 renames it, and the inspector's header says
    how many are selected;
  - after a click in the tree, the arrows move the selection (Shift extends it), Left
    closes a row (or goes to its parent), Right opens it (or goes to its first child),
    Shift+Right opens its whole branch;
  - the row's arrow opens or closes it;
  - **F2** or a double click renames it in place (Enter keeps, Escape drops);
  - the filter box keeps the rows whose names match, with their parents;
  - **`+`** (or Ctrl+A) adds an entity where the camera looks, on the grid: an empty
    entity, a cube, sphere, plane or cylinder, a sun, a point or spot light, a camera. It
    is named after its kind, numbered when a sibling has that name ("Cube 2");
  - Delete and Ctrl+D act on the whole selection (a child goes with its selected
    parent), as one undo step; each copy comes right after its original, numbered, and
    the copies are selected.
- **The inspector** edits the selected entity's components, field by field:
  - **numbers** are dragged sideways (see snapping below), or clicked and typed (see
    typing values below); vectors show their parts with colored `x y z` labels, colors
    `r g b a`;
  - **a rotation** shows as Euler angles in degrees (applied Y, X, Z: yaw, pitch, roll);
  - **checkboxes** for booleans, **a list** for enums;
  - **strings** (the name too) are typed in place;
  - references and containers show as text: asset pickers come with the asset browser
    (E4);
  - the filter box keeps the fields whose names match (and every field of a component
    whose name does);
  - **Escape** during a drag puts the value back;
  - **a changed field** (one that differs from its type's default) has a reset button
    (↺) at the end of its row; **Backspace** with the mouse over a field resets it too.
    A name has no reset;
  - **Ctrl+C and Ctrl+V** with the mouse over a field copy and paste its value (see
    copying values below).
- **The command palette** (Ctrl+Shift+P) finds and runs every command; Ctrl+P finds the
  scene's objects (see below).
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
| Rename the active entity | F2 |
| The command palette | Ctrl+Shift+P |
| Go to a scene object (the palette with `@`) | Ctrl+P |

After a click in the scene tree (its keys, rebindable like the rest):

| Keys | What they do |
|---|---|
| Up, Down | select the previous or the next row |
| Shift+Up, Shift+Down | extend the selection |
| Left | close the row, or go to its parent |
| Right | open the row, or go to its first child |
| Shift+Right | open the whole branch |

With the mouse over an inspector field that is not being typed in:

| Keys | What they do |
|---|---|
| Backspace | reset the field to its default |
| Ctrl+C | copy its value |
| Ctrl+V | paste a value into it |

While a field is typed in, the keys are the field's: Delete, Y or Ctrl+A edit the text,
and only Ctrl+S still saves. Text fields work as Windows' own: Shift and Ctrl with the
arrows, Home and End, Ctrl+A, Ctrl+C, Ctrl+X and Ctrl+V with the clipboard.

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

## The command palette

Ctrl+Shift+P opens a box at the top of the window: type part of what you want and Enter
runs the highlighted result.

- **Commands** are listed as "Category: Title" ("Scene: Duplicate Selection"), each with
  its shortcut beside it, so the palette also teaches the keys. The add menu's kinds are
  commands too: "Add Cube", "Add Point Light".
- **Matching:** the letters typed must appear in order; matches at word starts and runs
  of letters rank first, so "dup sel" finds "Duplicate Selection" and "apl" finds "Add
  Point Light". Matched letters are highlighted. Recently run commands rank higher among
  equals; with nothing typed they come first.
- **`@` finds the scene's objects** by name (Ctrl+P opens the palette with it typed); the
  one picked is selected, and the tree opens its parents' rows. `>` before a query looks
  for commands, as without it.
- Up and Down move the highlight, Enter or a click runs it; Escape or a click elsewhere
  closes the palette.

## Copying values

Ctrl+C over a field copies its value as a scene file writes it: `(0.25, 0, 0)` for a
vector, `true`, `Run` for an enum, `@<scene id>` for an entity reference; a string as it
is. Ctrl+V over a field reads the clipboard the same way, as one undoable step. Text that
is not a value of the field's type changes nothing; the status bar says why. A vector is
copied whole.

## Typing values

Click a number without dragging it and type; Enter keeps the value, Escape drops it.

| Typed | Gives |
|---|---|
| `2.5`, `-3e2` | the number |
| `2*pi`, `(1 + 2) / 4`, `2^10`, `sqrt(2)` | the expression's value (`pi`, `tau`, `e`; `sqrt`, `abs`, `floor`, `ceil`, `round`, `sin`, `cos`, `tan`, `min`, `max`) |
| `+=0.5`, `-=1`, `*=2`, `/=4`, `*1.2`, `/2` | the current value changed by it |
| `45deg`, `30°`, `pi/2 rad` | degrees, for angles (`rad` converts) |

A typed value that is not a number changes nothing; the status bar says why.

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
| E1-4b-3b | multi-editing: the selection's shared components, differing values read "—", relative edits per entity |
| E1-5 | sessions, autosave, rebuild and restart after a code change |
| E2 | selection in the viewport, gizmos, placement by drag and drop |
| E3 | Play on a copy of the world |

The whole plan is ADR 0107 §7.

## HDR

While Windows runs the display in HDR, the editor presents in HDR, as games do
([player.md](player.md#hdr)): the viewport's light rolls off towards the display's peak,
the panels stay at Windows' SDR white level. `--hdr scrgb|hdr10|off` picks the output
(scRGB by default).

## What it costs (ref-1, release, 1600×900)

| | |
|---|---|
| Start and 60 frames | 0.51–0.69 s (budget for the start alone: 2 s) |
| The editor's frame on the CPU (shell, tree of 200 entities, inspector) | 74 µs, 83–84 µs with a selection (92 µs with all 200 selected), 88 µs with the command palette open |
| The palette's `@` search over 10,000 objects, per keystroke | 0.62–0.73 ms |
