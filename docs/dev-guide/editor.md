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
  scene's objects, `#` the project's assets (see below).
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
| Open a scene in a tab ([scenes in tabs](#scenes-in-tabs)); a new empty one | Ctrl+O; Ctrl+N |
| Close the scene's tab; open the last closed again, with its unsaved changes | Ctrl+W; Ctrl+Shift+T |
| The next scene tab; the previous one | Ctrl+Tab; Ctrl+Shift+Tab |
| Delete the selection (with its children) | Delete |
| Turn the selection off, or on (with its children) | H |
| Duplicate the selection (with its children) | Ctrl+D |
| Add an entity | Ctrl+A |
| Snap by default, or not (Y is an axis in a keyboard transform) | G (Blender's keys: Shift+Tab; Godot's: Y) |
| Rename the active entity | F2 |
| The command palette | Ctrl+Shift+P |
| The Output (the engine's and your game's log), open or closed | Ctrl+J |
| Go to a scene object (the palette with `@`) | Ctrl+P |
| Find an asset (the palette with `#`) | Ctrl+Shift+P, then `#` |
| Rename the picked file (after a click in the FileSystem dock) | F2 |
| Where the picked asset is used | Shift+F12 |
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
| Edit the selected brushes' faces, edges and corners ([editing faces](#editing-faces)), with the pointer over the viewport | Tab |
| Drag the selection over surfaces | Ctrl+Shift with a move handle |
| What rotating and scaling turn about: the middle, each entity's own origin, the active entity | . (Period) |
| Play the scene, stop ([playing](#playing)) | F5, F8 |
| Play from the editor's camera; restart | Shift+F5; Ctrl+F5 |
| Pause or go on; step one tick | F7; Shift+F7 |
| Eject (the editor's camera and tools on the running game), return | Shift+F8 |
| While playing: free the mouse from the game, give it back | Shift+F1 |
| While playing: keep the selection's changes at Stop; pick from what changed | Ctrl+K; Ctrl+Shift+K |

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
- **`#` finds the project's assets** by path, a kind before a colon narrowing them
  (`#model:crate`); the one picked is shown in the FileSystem dock ([assets](#assets)).
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
- **Other brushes' corners and edges come first:** within a few pixels of the pointer the
  first corner lands on a corner (a green dot) or on an edge (a red ring; along an edge
  that runs along an axis, on the grid), the base's other corner on one as seen in the
  base's plane, and the height on a corner's when the pointer is over it. The hint says
  "on a corner" or "on an edge". So a new brush meets the old ones exactly.

### Editing faces

**Tab** with the pointer over the viewport edits the selected brushes' faces, edges and
corners (Blender's Tab; over the panels, Tab goes from field to field). The gizmo and the
handles hide; every face's edges show thin, the corners as small squares. What the pointer
takes lights blue: a corner within a few pixels of it, else an edge, else the face under
it.

| While editing | What happens |
|---|---|
| a drag on a face | moves it along its normal; the brush keeps its other faces. A move that would leave nothing of the brush stops before it |
| a drag on a corner or an edge | moves it (an edge's two corners together) in the plane of the brush's own axes that faces the camera most. The part is made again from its corners: corners that meet merge, so a box's top edge dragged down onto the bottom's makes a wedge, and a corner dragged inside goes |
| Alt+drag on a face | pulls a new brush out of it, as far as the pointer goes; pushed in, a cut of the face's outline that deep (a doorway pushed into a wall). It goes into the same group, and its faces are edited next too |
| Tab, Escape, a click on no face | ends the editing (the click selects what it hits) |
| Escape during a drag | takes the drag back |

- **An edited brush keeps its planes:** its shape (box, wedge, cylinder…) becomes plain
  planes (`BrushShape::Planes`), and its side handles go. Undo brings the shape back. A
  room stays a room: what moves is its inside's plane, the wall following; its corners
  are its walls', so they do not show.
- **Snapping** follows the rules above: the distance in whole steps with snapping on (G),
  Shift the other way, Ctrl rounds it; free, millimetres. The distance shows on the face
  while it is dragged.
- **Every drag is one undo step.**

### Exporting a greybox

**Blockout: Export to glTF** (the palette) writes the selected brush's group as
`<group>.glb` beside the scene, a part per tint over the grid ([blockout.md](blockout.md#exporting-a-greybox)).
The status bar says where and how many triangles.

### Baking a group

**Blockout: Bake to Mesh** (the palette; "bake", Enter) writes the selected groups (or
every group) as glTF files beside the scene and puts a model of each file's meshes under
its group, the brushes no longer drawn but still colliding, as one undo step. **Blockout:
Unbake** brings the brushes back ([blockout.md](blockout.md#baking-a-group)). While a
group is baked, Tab, Ctrl+H and the side handles leave its brushes alone and say so.

### Colliders

**View: Colliders** (the palette) draws the edges of the brushes' pieces that collide,
thin and cyan over the viewport: what the game's physics gets from them
([blockout.md](blockout.md#colliders)).

### Navigation

**View: Navigation** (the palette) bakes the navigation mesh of the brushes that collide
and draws it: its boundaries (walls, drops) bright green, the edges between its polygons
faint, for a human-sized agent (1.8 m tall, 0.4 m wide-radius, 0.4 m steps, 45° slopes;
[navigation.md](navigation.md)). It bakes in the background on workers of its own, a third
of a second after the brushes last changed, so editing never waits for it; the status bar
says how long it took ("Navigation: 357 tiles from 16,802 triangles in 158 ms").

### Sizing a brush by its sides

The active brush shows a small square in the middle of each side facing the camera. Drag
one: that side follows the pointer along its normal and the opposite side stays (a room's
sides are its inside's, its walls follow). Its size shows above the handle. Snapping as
above: whole steps of the size with G (or Shift), Ctrl rounds the size to the step, free
on the millimetres; never below a millimetre. One undo step per drag; Escape takes it back.
The handles come before the gizmo's under the pointer.
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

## Playing

**F5** (or ▶ at the end of the tab row) plays the scene: your game's `start` makes the
game on the scene's world, and its ticks run. **F8** (■) stops, and the scene is as it was
before Play.

**The game plays in a window of its own**, as large as the viewport, titled "main.scene
(playing)":
- the editor's viewport says where it is;
- F7 and F8 work in the game's window too, and closing it stops;
- the game gets its window's keys and mouse only while that window has the keyboard.

**"Play: In a Window or in the Viewport"** (the palette) plays it in the viewport instead.
The game plays there anyway beside frame generation or an HDR10 output, which a second
window cannot follow. While a game window is open, latency control (Reflex, Anti-Lag) is
off, until the next start (NVIDIA's driver crashed with Reflex beside a second window).

When Play starts:
- the session is saved first;
- the editor's camera steps aside, so the scene's own cameras show the game.

While playing:
- **The viewport shows the game**, inside a green edge, without the editor's gizmo,
  outlines or icons.
- **The tree and the inspector show the running world.** Edits apply at once and are the
  play's: Stop takes them back with the rest. Undo reaches the edits made while playing,
  not those from before.
- **Keys without Ctrl or Alt go to the game** (letters, arrows, Space, Delete); F1 to F12
  and Ctrl's chords stay the editor's.
- **Saving, restarting and baking wait for Stop**, and so do the autosave and a build's
  restart.

Stop puts back everything the game changed or added:
- the entities it spawned go;
- components it added go;
- moved and changed things come back as they were, despawned ones come back;
- resources it inserted go;
- its videos close, its voices stop and its network session ends.

Keeping the scene and putting it back take milliseconds: 7 and 8 ms for 10,000 entities.

### Pausing, stepping, playing from here

- **F7** (or ⏸) pauses: the world's time stands (its timers, tweens and sounds), your
  game's hooks are not called, and the frame shows where it stopped. F7 again goes on.
- **Shift+F7** runs one tick while paused (pausing first): one `Game::tick`, timers and
  tweens one tick on, then paused again.
- **Shift+F5** plays from the editor's camera:
  - the scene's main camera starts where the editor's camera is;
  - `engine_app::PlayFrom` (its position and rotation) is in the world while the game's
    `start` runs, for your game to put its player there.

  Stop puts both back.
- **Ctrl+F5** restarts: Stop and Play at once, from the camera again if the last Play
  was from there.
- **Shift+F8** ejects, Unreal's F8:
  - the editor's camera and tools come back on the running game, so you can fly around
    it, select and move things while it runs;
  - the game's keyboard and mouse are off meanwhile;
  - playing in its window, the view comes into the viewport, the game's window keeping its
    last frame.

  Shift+F8 again returns to the game.
- **The mouse is the game's** from Play on: the cursor is hidden and held over the game's
  view, as in the game. **Shift+F1** frees it (in the game's window too), and Shift+F1
  or a click in the game gives it back. Pausing, ejecting, and switching to another
  window free it as well; going on and returning give it back.
- **The game's window** opens at least 1280 × 720 the first time, and then where you left
  it.

### Keeping changes

What you change while playing goes away at Stop, unless you keep it. Changes the game
made count too: a door it opened, a crate it pushed, the things it spawned.

- **Ctrl+K** (Unreal's K) keeps the selection's changes: at Stop they go into the scene,
  and the tree shows "Kept" beside them meanwhile. Ctrl+K again on the same selection: not
  kept.
- **Ctrl+Shift+K** ("Play: Keep Changes…") lists what changed since Play: the scene's
  entities with the components that differ, the entities made while playing, and those
  despawned. Enter on a line keeps it or not; the first line keeps every change.
- **At Stop**, the scene is put back first; then the kept changes go in as one undo step,
  "Keep Changes":
  - a kept entity gets the values its saved components had (the game's own runtime
    components are not the scene's and stay out);
  - an entity made while playing becomes a new scene entity;
  - a kept despawn deletes the entity.

  Ctrl+Z takes the step back, and saving writes them.

**A panic in your game's code** stops Play instead of the editor. The status bar says the
message and where it happened (`the game panicked in `tick`: … (src/game.rs:42)`), and the
crash reporter still writes its report.

**While editing, the scene stands still:** tweens, timers and sounds start only with Play,
so editing never changes the scene behind your back.

## Assets

The editor keeps the project's asset database ([scenes.md](scenes.md#the-asset-database)):
every asset file with its id, a `.meta` beside each foreign file, the index in
`.ofelia/`. Files you add, move or delete in Explorer are followed a moment later, and
scenes find their assets by id. A model, material or image written anew (export from
Blender again, save a texture) is made again in the background, the old version drawn
until the new one is whole.

**The FileSystem dock**, under the viewport, shows them:
- on the left, the project's folders that hold assets, `res://` at the top;
- on the right, the folder's subfolders and files as tiles, each a thumbnail over its
  name.

A click on a folder opens it, a click on a file picks it.

- **Thumbnails:**
  - an image is its own picture made smaller;
  - a model is drawn by the engine with its own materials, seen from the front and a
    little above;
  - a material is drawn on a sphere;
  - other kinds show their icon.
- **Sizing:** Ctrl+wheel over the tiles makes them larger or smaller.
- **Renaming:** F2 after a click in the tiles renames the picked file in place (its
  extension is kept when you leave it out). A file dropped on a folder, a tile or a row
  of the folder tree, moves into it.
  - The `.meta` goes with the file, so it keeps its id and every scene keeps its
    references.
  - A scene saved after it writes the new path.
  - Neither is an undo step: they are files on disk.
- **Find References** (Shift+F12) lists where the picked asset is used, in the palette:
  - the open scene's entities, as the scene is now (Enter selects one);
  - then the project's scenes and materials on disk, with how many times (Enter shows the
    file).
- **Cache:** thumbnails are made in the background and kept in `.ofelia/thumbnails/`, and
  made again when their file is written anew.
- **Textures are compressed in the background:** the first time a model's textures are
  used, they show at once uncompressed, then are compressed as the shipped game's are
  (BC7, BC5 for normal maps) and swapped in, a quarter of the memory. The status bar
  shows "Compressing textures: N" meanwhile. They are kept in `.ofelia/ddc/`, so the next
  time the project opens they load compressed at once. Deleting that folder only costs
  compressing them again.
- **Import settings:** a file picked in the dock is shown in the inspector, its path and
  kind, then its settings, until you select entities again:
  - a model: **scale** (0.01 for a file made in centimeters) and **texture compression**
    for the images inside it;
  - an image: **compression**, `Auto` (BC7, BC5 for normal maps) or `Uncompressed` (pixel
    art, gradients, text).

  A change is saved in the file's `.meta` at once and the asset is made again, with
  nothing to press. It is not an undo step (a file on disk, as a rename): ↺ puts a
  setting back.

**`#` in the command palette** finds assets by path: `#crate`, or with a kind first,
`#model:crate`, `#texture:rock` (`scene`, `material`, `model` or `mesh`, `texture`, `sound`
or `audio`, `video`, `font`, `script`). Enter shows the asset in the dock.

**Drag a file out of the dock** to use it; the hint by the pointer says what letting go
will do:

| Drop | Onto | What happens |
|---|---|---|
| a model (`.glb`, `.gltf`) | the viewport | it rests on the surface under the pointer while you move, and stays where you let go |
| a model | a row of the scene tree | it goes under that entity, at its origin |
| a model | the tree below its rows | a new root where the camera looks |
| a material (`.material`) | an object in the viewport, or its row | the object's model takes it |
| an asset | an asset field in the inspector | the field takes it, on every selected entity with that component |

- **Placing in the viewport:**
  - the model's box rests on the surface: its bottom on a floor, its side on a wall;
  - where nothing is under the pointer, it stands on the ground's plane (y = 0);
  - with snapping on, its place along the surface snaps to the position step (Shift
    inverts it, as for drags).
- **Escape**, or moving back out of the viewport, takes the model away again. Every drop
  is one undo step.
- **What a model becomes:**
  - a file of one node is one entity, named after the file;
  - otherwise a root named after the file, with an entity for each node of the file's
    scene (its transform, and a model of its mesh and material).
- **Fields:** a model's `mesh` takes a model file of one mesh, its `material` a material
  file or a model file of one material.

**Asset fields in the inspector** show the asset as Godot's do:
- a square preview the engine draws: a mesh with its own material, a material on a
  sphere, an image as itself;
- the asset's name beside it.

A click opens the palette on the project's assets of the field's kind (`#model:`,
`#material:`), and Enter puts the one picked in the field, on every selected entity. A
file dropped on the field takes its place.
- **Meshes of several materials** draw each primitive with its own: the drop gives the
  model the further primitives' materials in its `MaterialSlots` (ADR 0115). Slots are
  not editable in the inspector yet; the scene file holds them as a list.

## Prefabs

Any scene can be placed in another ([scenes.md](scenes.md#prefabs)): a crate, a lamp, a
room.

- **Place one:** drag a `.scene` from the FileSystem dock into the view (its root lands
  on the surface under the pointer, Esc takes it back) or into the tree (under the row it
  is dropped on, or after the roots). It is an instance: the tree calls its root
  "Prefab".
- **Make one** (Ctrl+Shift+P, "Scene: Make Prefab"): the selected entity and its children
  are written to a scene of their own in the dock's folder (named after the entity), and
  become an instance of it, in one undo step.
  - The file's root sits at its origin; the instance keeps its place as its own change.
  - An instance made a prefab is a variant of its prefab.
  - Undo puts the plain entities back; the file stays.
- **Unpack one** ("Scene: Unpack Prefab" on an instance's root): its entities become plain
  entities, one undo step; instances inside it stay instances.
- **What is saved:** an instance writes only what you changed from its prefab. Change the
  prefab and every instance follows, except where it says otherwise.
- **Overrides in the inspector:** select an instance (the inspector's title says
  "instance of props/lamp.scene") or an entity in one ("in props/lamp.scene"). A field is
  marked changed (↺) where it differs from the prefab's value: its name and its assets
  count too.
  - ↺, or Backspace over the field, puts the prefab's value back.
  - "Prefab: Revert Overrides" puts the whole instance back to its prefab's values, in
    one undo step, and takes away the components it added. Entities and components it
    removed stay removed.
  - "Prefab: Apply Overrides" writes the instance into its prefab: its values, and the
    components and entities it added or removed. The prefab's other instances follow at
    once. Undo puts the scene back; the file stays as written.
- **Edit the prefab itself:** "Prefab: Open", or Open in the inspector's title beside
  "instance of…" or "in…", opens the prefab of the nearest instance in a
  [tab](#scenes-in-tabs) of its own, the entity you had selected selected there (an
  instance's root: the prefab's root). Edit it as any scene and press Ctrl+S: the other
  tabs' instances take it when you go back to them.
- **A prefab written anew** (by Apply, by its own tab, by another editor, by hand) is
  followed by the open scene's instances: they take its new values where they did not
  change them, its new entities and components, and lose what it no longer has where they
  kept it as it was. What they changed stays theirs. While the scene plays, this waits
  for Stop.

## The Output

**Ctrl+J** (or the Output tab in the bottom bar) opens the log under the viewport. It holds
what the engine and your game log through the `log` crate's macros, from the editor's
start (`log::warn!(target: "doors", "door {id} is stuck")`).

- **Each level's toggle** in its header shows how many came: errors, warnings, information,
  and debugging (off until you turn it on). The filter keeps the rows whose target or
  message holds what you type.
- **A message that comes again** folds into its row, with a count (×30): an error each
  frame takes one row.
- **A line marks each Play's start.** Follow keeps the newest row in view, and Clear empties
  the panel.
- **The tab** says how many errors came, even while the panel is closed.
- **A row that names its cause** carries a link sign; a click shows it:
  - a scene entity's id (32 hex digits) selects it, and an asset's id shows it in the
    FileSystem dock;
  - a project file's path (`props/lamp.scene`) shows it in the dock;
  - a file and line (`src/main.rs:12:5`, `scripts/door.luau:7`) opens in VS Code.

  Your own messages work the same: name the file or the entity's id in the text
  (`log::warn!("{}: no door here", path)`). A failed build of your game's code puts each
  error there too.

## Tasks and History

- **Tasks** (the bottom bar's second tab, or "View: Tasks"): what runs in the background
  and how long it has run. That covers your game's build, assets loading, textures
  compressing, thumbnails and the navigation bake. The tab counts them; you keep working
  meanwhile.
- **History** (the scene dock's second tab, or "View: History"): every step by name, the
  current one highlighted, those undone below it dimmed, another tab's steps with their
  scene's name. A click goes back or forward to that step; `Start` takes back everything.

## The profiler's panel

The bottom bar's **Profiler** tab ("View: Profiler") shows how long the frames take:

- the latest frame's CPU and GPU milliseconds;
- a bar a frame for the last 120, those longer than a 60 Hz frame (16.7 ms) in yellow;
- the costliest CPU stages (simulate, extract, record, submit, …) and GPU passes (shadows,
  forward, bloom, …) of the latest frame, a pass that runs several times summed with its
  count.

For a whole timeline, run under Tracy with the profiling build ([profiling.md](profiling.md)).

## Scenes in tabs

Several scenes open at once, each in a tab above the viewport (ADR 0107 §4), with Godot's
keys:

- **Ctrl+O** ("File: Open Scene…") lists the project's scenes in the palette; Enter opens
  one in a tab after the one shown. A double click on a scene in the FileSystem dock does
  the same. A scene already open shows its tab.
- **Ctrl+N** opens an empty untitled scene; Ctrl+S names it (`untitled.scene`) and writes
  it.
- **A click on a tab**, Ctrl+Tab and Ctrl+Shift+Tab show another. The scene shown last
  waits as it was: its unsaved changes (the tab keeps its `*`), its camera, selection and
  tree. A scene with nothing unsaved whose file changed on disk meanwhile is read again.
- **One history for every tab:** Ctrl+Z takes back the last change made, in whichever
  tab it was made, and shows that tab first.
- **Ctrl+W** (or a tab's ×) closes it without asking: its unsaved changes stay in its
  autosave, and **Ctrl+Shift+T** ("File: Reopen Closed Scene") brings it back with them,
  as does opening the file again. Its changes leave the undo history. The last tab stays.
- **The session keeps the tabs:** the next start opens them in their order, each with
  its unsaved changes and its view, and shows the one you were in.
- **While playing,** the scene that plays stays in front: Stop first.

Switching costs about 19 ms for a scene of 10,000 entities shown again, 11 ms to leave it
for a small one (ref-1, release): the scene left is kept as its text and the other loaded.

## Scripts

Scripts open in tabs after the scenes' (ADR 0107 E8), their code where the viewport is:

- **Opening one:**
  - a double click on a `.luau` file in the FileSystem dock;
  - "File: Open Script…" in the palette;
  - a click on an error at a script's line in the Output, which opens it at that line.
- **A scene's tab** brings the scene back in front; Ctrl+Tab goes through the scenes'
  tabs, then the scripts'.
- **The keys are VS Code's:**
  - Ctrl+D selects the word, then its next match, each with a caret of its own;
    Alt+click and Ctrl+Alt+Up or Down add carets; Escape leaves one;
  - Ctrl+/ comments lines, Alt+Up and Alt+Down move them, Ctrl+Shift+K deletes them;
  - Ctrl+C or Ctrl+X without a selection takes the whole line, and Ctrl+V puts it back
    above yours;
  - Enter keeps the indentation, one level more after `then`, `do` or `function (…)`;
    Tab and Shift+Tab indent;
  - Ctrl+Z undoes the script's own changes, not the scene's.
- **While the code has the keys,** the scene's shortcuts are off. The editor's own still
  work: Ctrl+S, the palette, the tabs, the Output (Ctrl+J), F5 to F8. Click a panel to
  give the keys back to the editor.
- **Ctrl+S** saves the script, also while the game plays. The file keeps its line endings.
- **Closing a script** with unsaved changes takes a second Ctrl+W.
- **Edits made elsewhere** (in VS Code) show up when the tab has nothing unsaved.
- **The session keeps your scripts:** the next start opens their tabs again, with their
  carets and their unsaved text.

Highlighting, find and replace, and Luau's errors and completion as you type come next.

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
| E3 | Play: the tree's marks on what changed (the rest of Play is done) |
| E7 | Luau in the game: the script component, its hooks, bindings and hot reload (its public API waits for the team's answers, ADR 0114) |
| E8 | the code editor: highlighting, find and replace, Luau's errors and completion as you type (scripts in tabs are done) |

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
