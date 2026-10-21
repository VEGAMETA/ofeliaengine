# The editor

The editor runs inside your game's own build (ADR 0107): the player runs the game's world,
draws the scene into the viewport panel, and the editor's shell (the team's Godot layout)
is drawn over the frame. Your components and systems are native in it.

In your game's folder (a game made by `ofeliaengine new` has the `editor` feature):

```text
cargo run --features editor
```

`ofeliaengine edit [DIR]` starts it too, and keeps the console with the editor; when the
first build fails, it starts the last good one. The editor opens the scene of the game's
settings (`PlayerSettings::scene`), and rebuilds the game when its code changes
([below](#code-changes)).

Without a game, the editor's own binary opens a scene file:

```text
cargo run -p engine_editor --release --bin editor -- scenes/main.scene --project C:/games/mygame
```

- **`scene`**: a scene file (ADR 0108), relative to `--project` (the working folder by
  default). Without one, an empty world.
- **`--frames N`** renders N frames and exits (smoke tests), printing the time with the
  start.

**How a game runs it:** the game's `main` is `engine_app::main!(settings, start)`
([player.md](player.md#your-games-main)). With the game's `editor` feature it calls
`engine_editor::run(settings, start)`; without it, the player, and a shipped game does
not link the editor. `start` makes the game from the player when Play starts (Play comes
with phase E3). The manifest, as `ofeliaengine new` writes it (with the engine's path):

```toml
[dependencies]
engine_app = { path = "../ofeliaengine/crates/engine_app" }
engine_editor = { path = "../ofeliaengine/crates/engine_editor", optional = true }

[features]
editor = ["dep:engine_editor"]
```

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
  - **the eye** at the row's end turns the entity off and on, as Godot's: off, it and
    what is below it are not drawn, light nothing and leave their blockout groups, and
    the scene keeps it so (`Disabled`); the rows that are off are dim. **H** does the same
    for the selection (any of it on: all off; all off: all on), one undo step;
  - **F2** or a double click renames it in place (Enter keeps, Escape drops);
  - the filter box keeps the rows whose names match, with their parents;
  - **`+`** (or Ctrl+A) adds an entity where the camera looks, on the grid. Its menu
    groups the kinds; a group opens beside it under the pointer:
    - **Point** (an entity with a place and nothing else), **Camera**;
    - **Mesh:** a cube, sphere, plane or cylinder;
    - **Blockout:** brushes ([blockout.md](blockout.md)): a box, wedge, stairs, cylinder,
      cone, truncated cone, sphere or torus. A brush joins the selection's blockout group
      (or the scene's first), or a new "Blockout" group, at its end;
    - **Light:** a sun, a point light or a spot light.

    It is named after its kind, numbered when a sibling has that name ("Cube 2"); the
    palette has every kind ("Scene: Add Torus Brush");
  - Delete and Ctrl+D act on the whole selection (a child goes with its selected
    parent), as one undo step; each copy comes right after its original, numbered, and
    the copies are selected.
- **The inspector** edits the selected entities' components, field by field. With
  several selected:
  - it shows the components they all have; a value that differs between them reads "—"
    (a vector part by part: only the `x` that differs);
  - a typed value goes to all of them; a relative one (`+=1`, `*2`) and a drag move each
    from its own value; a checkbox that differs turns all on; reset and paste act on all;
  - each edit is one undo step for all of them.

  For each field:
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
- **Saving** writes the scene file under the scene's own id, asset references by path,
  the roots in the tree's order (`order` lines: [scenes.md](scenes.md#versions)).
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
| Turn the selection off, or on (with its children) | H |
| Duplicate the selection (with its children) | Ctrl+D |
| Add an entity | Ctrl+A |
| Snap by default, or not (Y is an axis in a keyboard transform) | G (Blender's keys: Shift+Tab; Godot's: Y) |
| Rename the active entity | F2 |
| The command palette | Ctrl+Shift+P |
| Go to a scene object (the palette with `@`) | Ctrl+P |
| Move, scale, rotate the selection at once ([keyboard transforms](#keyboard-transforms)) | Q, E, R |
| The gizmo's handles: move, scale, rotate (the same again: all of them) | Shift+Q, Shift+E, Shift+R |
| The gizmo along the active entity's axes, or the world's | Space |
| Fly the camera ([viewport navigation](#viewport-navigation)) | the right button held + W, A, S, D |
| Frame the selection (nothing selected: the scene) | F |
| Follow the active entity as it moves (again: stop) | Shift+F |
| Set a camera bookmark, go to it | Ctrl+1..9, 1..9 |
| Put the selection on the surface below it | End |
| Draw a brush, a cut ([drawing brushes](#drawing-brushes)) | B, Shift+B |
| Make the selected brushes rooms (walls 0.2 m thick), or solid again | Ctrl+H |
| Drag the selection over surfaces | Ctrl+Shift with a move handle |
| What rotating and scaling turn about: the middle, each entity's own origin, the active entity | . (Period) |

These are our keys, the default (team decision, 2026-10-21): the left hand stays beside
W, A, S, D, which fly the camera. **Editor: Key Map…** (Ctrl+Shift+P) takes another set,
and the session keeps the choice:

| Key map | Transforms at once | The gizmo's tools |
|---|---|---|
| ours | Q move, E scale, R rotate | Shift+Q, Shift+E, Shift+R |
| Blender's | G move, R rotate, S scale | Q select, W move, E rotate, T scale |
| Godot's | G move (then R rotates, S scales) | Q select, W move, E rotate, R scale |

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

While a field is typed in, the keys are the field's: Delete, G or Ctrl+A edit the text,
and only Ctrl+S still saves. Text fields work as Windows' own: Shift and Ctrl with the
arrows, Home and End, Ctrl+A, Ctrl+C, Ctrl+X and Ctrl+V with the clipboard.

Keys are physical positions, so they stay put on any keyboard layout. A keyboard
transform's X, Y and Z (and the transforms' own letters while it runs) go by the letter on
the key instead: Z is the key that says Z on a German keyboard too, and in a Cyrillic
layout the key's Latin letter counts. Every command has a
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
- The grid button in the tool bar (or G) makes snapping the default; Shift then drags
  freely.
- **The steps sit beside it** (as in Unreal's tool bar): position, angle and scale, each
  a menu of values (0.01 to 5 m, 1° to 90°, 0.01 to 1), the current one checked; dim
  while snapping is off. The palette has them too ("Snapping: Position Step…" and the
  others), and the session keeps them.
- The viewport's gizmo and keyboard transforms follow the same rules
  ([below](#moving-things-in-the-viewport)): a move in steps of 0.1 m, a turn of 45°, a
  scale of 0.1; with Ctrl, the selection's center lands on the grid. Typed values are
  exact.

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

## Picking in the viewport

- **A click** on an object in the viewport selects it, and the tree opens to its row. A
  click misses nothing thinner than 3 pixels: the nearest object within them is taken.
- **Shift+click** (or Ctrl+click) adds an object to the selection, or takes it out when it
  is already selected. In the scene tree Shift selects a range and Ctrl one row, as in
  every list.
- **A click on nothing** clears the selection; with Shift or Ctrl it keeps it.
- **Clicking again at the same spot** selects the object behind the active one, then the
  one behind that, and round to the nearest: what a nearer object hides is a click away. A
  Shift or Ctrl click, or a click elsewhere, starts over.
- **Alt+click** lists everything under the pointer in the palette, nearest first, hidden
  objects too: pick one to select it, or type to narrow the list.
- **A box:** press anywhere in the viewport but on the gizmo's handles and drag. It takes
  every object it shows a pixel of, outlined thinly while you drag; at the release it
  becomes the selection, with **Shift** it is added, with **Ctrl** taken out (those it
  would take out lose their orange line meanwhile). With **Alt** (a dashed frame) it takes
  hidden objects too. Escape takes the box back. The nearest object taken ends active.
- A press on one of the gizmo's handles drags it instead, and a press that moves more than
  4 pixels draws a box instead of clicking. The select tool's middle handle sits at the
  object's middle: to click there again, click beside it, or use a tool without it
  (rotate).
- What is picked is what is drawn: masked surfaces through their holes, skinned meshes in
  their current pose, transparent ones too. **Viewport: Pick Transparent Surfaces** in the
  palette turns the last off, so clicks go through glass; the session keeps it.
- **Lights and cameras** show as icons in the viewport (a bulb, a spot, a sun, a camera on
  a dark disk), above the scene: a click on one selects it, a box takes the icons in it,
  and the one under the pointer turns white (the selected ones orange). A plain click
  again on the icon it selected goes through to what lies under it, as cycling does.
  Empty entities have nothing to show: pick them in the tree.
- **Their reach:** a selected point light shows its range as three circles, a spot light
  its cone, a camera its view's pyramid (2 m deep), the sun its direction.
  **View: Overlays** in the palette hides icons and reach; the session keeps it.
- A click answers a frame or two later: the editor draws the objects whose bounds reach the
  pixels around it once more, each pixel with the object it shows (ADR 0109). The pointer's
  pixel is drawn again whenever the pointer, the camera or the scene moves.
- **Outlines:** the selection is outlined in orange, 2 pixels wide, the active entity in a
  lighter orange; an entity's children are outlined with it. The object under the pointer
  gets a thin light line, so you see what a click will take. Where something else hides an
  outlined object, its line is drawn at a third of its strength.

## Moving things in the viewport

The selection's gizmo sits at its center: the middle of the selected entities, a child
moving with its selected parent.

| Tool | Key (Blender's, Godot's) | Handles |
|---|---|---|
| Select | the active tool's key again (Q, Q) | all below: arrows, squares, rings, and the scale boxes halfway along the arrows |
| Move | Shift+Q (W, W) | an arrow per axis, a square per plane, a dot in the middle for the view's plane |
| Rotate | Shift+R (E, E) | a ring per axis, the half that faces you |
| Scale | Shift+E (T, R) | a line and a box per axis, a box in the middle for all three |

- **Drag a handle** with the left button. The handle under the pointer is drawn yellow,
  and the status bar shows the change ("Move (0.3, 0, 0)", "Rotate 45°").
- **Snapping** follows the rules above: Shift for steps, Ctrl for the grid, G to make
  steps the default.
- **Escape** during a drag puts everything back. Each drag is one undo step.
- **On surfaces:** hold **Ctrl+Shift** while dragging an arrow, a square or the middle:
  the selection slides over the surface under the pointer, its box's face on it (a crate
  on a shelf stands on it, against a wall it leans on it). What is selected is left out
  of what it can stand on.
- **End** puts each selected entity on the surface below it: its box's bottom on the
  first surface under the bottom's middle, the selection left out, as one undo step.
  "Nothing below" when there is none.
- **Space** turns the gizmo to the active entity's axes, and back to the world's.
- **The pivot** (the tool bar's right end, or `.`): rotating and scaling turn about the
  selection's middle (the default), about each entity's own origin (the gizmo stays in
  their middle; moves are the same), or about the active entity, where the gizmo then
  sits. Keyboard transforms follow it too; the session keeps it.
- **Exact values:** only what changes is written, from the entity's own values: a move
  sets the position, a turn the rotation, a scale the scale. The scene file gets no
  float dust (`0.99999994`). A scale goes along the entity's own axis nearest the
  handle's.
- Handles seen edge-on are left out: a plane square from the side, a ring seen as a line,
  an arrow pointing at you.

## Keyboard transforms

**Q, E or R** moves, scales or rotates the selection with the pointer at once: there is no
handle to grab (Blender's way, on G, R, S in its key map). The gizmo hides meanwhile. A hint by the pointer says what
it does ("Move along Y: 2 m", "Rotate about Y: 45°  ·  in steps"), and the status bar lists
the keys.

| While it runs | What happens |
|---|---|
| the pointer moves | a move follows it in the view's plane; a turn follows its angle around the pivot (past half a turn too); a scale follows its distance from the pivot |
| X, Y, Z | keeps it to that axis, drawn across the viewport in the axis' color. The same key again takes the active entity's axes (the world's, after Space); a third time frees it |
| Shift+X, Y, Z | keeps it to the plane across that axis: a move in that plane, a scale along its two axes |
| a typed value | metres, degrees or a factor, an expression too (`2*1.5`, `-90`, `(1+2)/4`); it takes the pointer's place. Backspace edits it, and `,` is a decimal point |
| Q, E, R | starts another transform over from here (G, R, S in Blender's and Godot's keys) |
| Enter, a left click | keeps it, as one undo step |
| Escape, a right click | puts everything back (leaving the window does too) |

- **Without an axis,** a typed move goes along x, a typed turn about the view's axis, and a
  typed scale is uniform. A typed turn about an axis is counterclockwise looking down
  that axis.
- **The pointer follows the snapping rules** above: Shift for steps, Ctrl for the grid.
  Typed values are exact: `R Y 90` leaves exactly a quarter turn in the file, `E 0.5`
  exactly half the scale.
- **Examples:** Q Y 2 Enter moves a crate 2 m up (y is up); R Y 90 Enter turns it a
  quarter about the vertical; E Shift+Y 2 Enter doubles its width and depth and keeps
  its height.
- The palette has them as commands too: "Transform: Move Selection", "Rotate Selection",
  "Scale Selection".

## Drawing brushes

**B** draws a blockout brush on the surface under the pointer, **Shift+B** a cut
([blockout.md](blockout.md)); the status bar lists the keys, and a hint by the pointer
names the shape and its sizes ("Stairs 1 × 1.8 × [1.2] m, 6 steps of 0.2 m").

| While drawing | What happens |
|---|---|
| before the first click | a mark shows where the first corner lands: on the surface under the pointer (the selection's too), else on the ground. The camera still moves (the right button, the middle one, the wheel) |
| B again, before the first click | the next shape: box, wedge, stairs, cylinder, cone, truncated cone, sphere, torus; Shift+B makes it a cut, B a brush again |
| a click | the first corner; then the base follows the pointer in the surface's plane, and a second click (or a press dragged away and released) keeps it |
| the pointer, then | the height follows it along the surface's normal, either way; a click keeps it and makes the brush |
| a typed size | the base's first side, Tab, the second, Enter; the height, Enter. Metres and expressions, as in keyboard transforms. A cut's typed height goes into the surface, a brush's out of it |
| R | turns the shape a quarter about the normal: wedges and stairs rise away from the first corner along the second side, until turned |
| Ctrl+H | makes it a room: what you draw is its inside, its walls 0.2 m thick around it, and it opens into the rooms it reaches ([blockout.md](blockout.md#rooms)) |
| Escape, a right click | drops it (leaving the window does too) |

- **The brush is there as you draw it,** cutting and cut as it will be: a doorway opens
  in the wall as it deepens. The box's edges and sizes show over it, a cut's dashed and
  orange.
- **Where it goes:** into the selection's blockout group, else the scene's first, else a
  new "Blockout" group, at its end; one undo step, and it ends selected.
- **Snapping** follows the rules above: with snapping on (G), or Shift with it off, the
  first corner lands on the grid of the position step in the surface's plane and sizes go
  in whole steps; Ctrl rounds the corner under the pointer to the grid. Free, corners and
  sizes still land on the brushes' millimetre grid.
- **Stairs** get as many steps as make each about 0.2 m high.
- The palette has them as "Blockout: Draw Brush", "Blockout: Draw Cut" and "Blockout: Room or
  Solid".

## Viewport navigation

| Action | Input |
|---|---|
| Orbit around the pivot | middle drag |
| Pan | Shift + middle drag |
| Zoom toward the pivot | mouse wheel |
| Look around (the pivot follows) | right drag |
| Fly | the right button held + W, A, S, D along and across the view; Q down, E up; Shift three times faster |
| The flight's speed | the wheel while flying: 5 m/s at first, ×1.2 a notch |
| Frame the selection | F: the camera flies in a fifth of a second to look at it (with its children) the same way, filling the view |
| Follow it | Shift+F: the camera moves with the active entity until you pan, look around or fly |
| Bookmarks | Ctrl+1..9 keeps the camera's place as bookmark 1..9, the digit flies back there (as in Unreal); the session keeps them with the scene |

The bindings follow Godot (`docs/research/ux/viewport-navigation.md`). While the right or
the middle button drags the camera, the cursor is hidden and stays where the drag began
(as in Godot and Unreal), so the mouse can go on without reaching the screen's edge. While
the right button flies the camera, the letters are the flight's: no shortcut runs. W, A, S, D, Q and
E go by their place on the keyboard (Z, Q, S, D on AZERTY). Framing takes each object's
bounds as the renderer has them, and lights, cameras and empty entities as points with
half a meter around them; nothing selected, it frames the whole scene.

## Autosave and the session

The editor keeps its own files in the project's `.ofelia/` folder (ADR 0107 §4). They are
never committed: `ofeliaengine new` ignores the folder. Your scene file changes only when
you press Ctrl+S.

- **Unsaved changes are autosaved** 30 s after the first one, at a quiet moment: never
  during a drag or while you type, and at the latest 30 s later. Each scene keeps its
  latest autosave and two older ones (`.ofelia/autosave/<scene>-<id>.scene`, `.1`, `.2`),
  at least 30 s apart. Once the scene is clean again (saved, or undone back to its file),
  its autosaves go.
- **Closing the editor asks nothing.** Unsaved changes stay in the session, and the next
  start brings them back, with the tab's `*`. After a crash they come back the same way,
  with at most the last 30 s or so lost; the status bar says so.
- **The view comes back too:** the editor's camera, the selection (the active entity
  last), the tree's closed rows, the filters, the tool, snapping and its steps, the key
  map, the flight's speed, and the palette's recently run commands. They are written when they change, at most every 10 s
  at a quiet moment, and at the exit.
- **Scene: Restore Unsaved Changes…** (Ctrl+Shift+P) lists the scene's autosaves, the
  newest first: the latest, the two older ones, and the one kept when the disk version
  was taken. The one you pick replaces the scene as one undo step.
- **When the file changed on disk** since your unsaved changes (a teammate's commit, a
  pull), neither is written over:
  - your version comes back, the tab shows a warning sign, and the status bar names the
    file until you choose;
  - Ctrl+S writes nothing and says why;
  - **Scene: Take Disk Version** (Ctrl+Shift+P) loads the file as one undo step: Ctrl+Z
    brings your version back. Your autosave is kept as `….conflict.scene`;
  - **Scene: Save Over Disk Version** writes yours over it.
- **One editor per project:** a second one on the same folder stops with "another editor
  has this project open".
- **If a start crashes while it brings changes back** (within its first 10 s), the next
  one opens the file as saved and says where the changes are kept.

What it costs (ref-1, release): the autosave's text is made on the editor's thread, the
writing happens on a thread of its own. 0.15 ms for a scene of 200 entities, 8.9 ms for
10,000 (once per 30 s at most, at a quiet moment).

## Code changes

The editor rebuilds your game while it runs (ADR 0107 §1):

- **A change to its code** starts `cargo build --features editor` in the background,
  below normal priority. That is a change in `src/`, in `Cargo.toml`, or in the engine's
  sources the game was built from. The status bar shows the build, and a change during
  it builds again after it.
- **Errors** show in the status bar, the first one with its file and line. **Build: Show
  Errors…** (Ctrl+Shift+P) lists them all. The one you pick opens at its line in VS Code
  (`code` on `PATH`); without VS Code, its place goes to the clipboard.
- **The new build restarts the editor** with the session as it was: the scene's unsaved
  changes, the camera, the selection.
  - While the editor's window is in the background (you are typing code), it restarts
    by itself.
  - While the window is in front, it offers the restart: **Ctrl+R** (Editor: Restart with
    the New Build).
  - **Editor: Restart Policy…** makes it always restart by itself, or always ask.
  - **No gap:** the editor's window stays until the new build has drawn its first frames.
    The new window opens hidden at the old one's place (maximized if it was), shows those
    frames, and takes the keyboard if the old window had it. Then the old editor exits.
  - **A new build that fails at its start** (a panic in `settings()`, a crash) leaves the
    editor running. The status bar says why: "Not restarted: the new build stopped at its
    start (exit code: 101); this editor goes on".
- **The window opens where it was:** the session keeps its place, for any start.
- **Builds use the dev profile** `ofeliaengine new` writes (team decision, 2026-10-20):
  - your code at `opt-level` 1, with file and line debug info; the engine at 3, without
    debug info;
  - on ref-1 a change to the quick start's code rebuilds in about 1.5–1.6 s (release: 3.0 s),
    and the restart takes 0.36–0.37 s;
  - your code runs slower in the editor than in a release build, so measure your game's
    speed in a release build.
- **The editor runs a copy of the build** from `.ofelia/run/`: three slots, each the
  executable and its PDB. Windows keeps a running program's file locked, and cargo must
  write its own. Run under `cargo run --features editor`, the editor moves into a slot
  before its window opens.

## Coming next

| Phase | What |
|---|---|
| E2b | blockout: brushes for greyboxing a level (box, wedge, stairs, cylinder; add, subtract), their colliders and navigation mesh |
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
| The editor's frame on the CPU (shell, tree of 200 entities, inspector) | 74 µs, 84 µs with a selection (105 µs with all 200 selected and compared), 88 µs with the command palette open |
| The palette's `@` search over 10,000 objects, per keystroke | 0.62–0.73 ms |
| The editor's frame during a keyboard move (the pointer moving, the undo step set every frame) | 103.5 µs (89.9 µs with one selected and its gizmo, 80.3 µs with nothing selected) |
