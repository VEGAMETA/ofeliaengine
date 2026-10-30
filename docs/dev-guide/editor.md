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
  - **drag rows to move them,** the selection with them:
    - onto a row's middle (outlined): they become its children, last;
    - near a row's top or bottom edge (a line shows): before or after it, among its
      siblings (the roots too);
    - below the rows: last among the roots;
    - each stays where it is in the world; hold **Alt** to keep its local placement
      instead;
    - one Ctrl+Z puts them back;
    - a collapsed row opens when you hold them over it;
    - a part of a prefab instance keeps its place (unpack the instance to move it);
    - brushes moved to another blockout group join it at once;
  - the filter box keeps the rows whose names match, with their parents;
  - **`+`** (or Ctrl+A) adds an entity where the camera looks, on the grid. Ctrl+A opens
    its menu under the pointer, kept inside the window with room for a group beside it;
    `+` opens it under itself. The menu groups the kinds; a group opens beside it under
    the pointer:
    - **Point** (an entity with a place and nothing else), **Camera**;
    - **Mesh:** a cube, sphere, plane or cylinder;
    - **Blockout:** brushes ([blockout.md](blockout.md)): a box, wedge, stairs, cylinder,
      cone, truncated cone, sphere or torus. A brush joins the selection's blockout group
      (or the scene's first), or a new "Blockout" group, at its end;
    - **Light:** a sun, a point light or a spot light;
    - **Sound:** a sound heard as it is (`AudioSource`: music, the UI), a sound in the
      world (`AudioSource3D`), the listener (`AudioListener`) or a reverb zone
      (`ReverbZone`). A new source is silent until the inspector gives it a sound
      ([audio.md](audio.md)). A reverb zone added while something is selected takes the
      selection's box: select a hall's brushes, add one, and it fills the hall (one undo
      step).

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
  - **a transform's scale has a lock** beside its name, shut at first. Shut, a part dragged
    or typed takes the other two along in proportion (from a zero, they take its value), as
    Unity's constrained proportions and Unreal's scale lock do. A click opens it, and each
    part goes on its own. The session keeps it;
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
- **Components and scripts** (ADR 0114 E7-4a):
  - **Add Component** (the button under the components, or Ctrl+Shift+A) lists the
    components a selected entity lacks, by name: the engine's, your game's, and your
    scripts' with their files. Pick one: each selected entity that lacks it gets it at its
    default, as one undo step. New Script… ends the list;
  - **the × on a component's header** takes it from the selected entities, as one step.
    A transform has none: it places the entity;
  - **Attach Script** (the script button beside the scene dock's +, as in Godot) lists New
    Script… first, then your scripts' components;
  - **New Script…** writes `scripts/<name>.luau` from a template, named after the active
    entity: `Front door` gives `scripts/front_door.luau` declaring `FrontDoor`, with a
    number when the name is taken. At the next frame it is loaded, its component goes to
    the selection as one step, and it opens with the caret in `on_tick`. Undo takes the
    component off; the file stays;
  - **a script's component** shows its file under its header, and its entity's row ends in
    the script's mark. A click on either opens the script; an entity with several scripts
    offers them in the palette;
  - **a saved script runs again** at the next frame, while playing too: its instances go
    on with their state, its props' values kept by name, and undo goes on working
    (`scripting.md`, "Changing a script while it runs").
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
| Add an entity (the menu under the pointer) | Ctrl+A |
| Add a component to the selection | Ctrl+Shift+A |
| View mode: Lit, Unlit, Wireframe, Lit + Wireframe | Alt+4, Alt+3, Alt+2, Alt+5 |
| The blockout's colliders shown, or not | Alt+C |
| Snap by default, or not (Y is an axis in a keyboard transform) | G (Blender's keys: Shift+Tab; Godot's: Y) |
| Rename the active entity | F2 |
| The command palette | Ctrl+Shift+P |
| The Output (the engine's and your game's log), open or closed | Ctrl+J |
| Go to a scene object (the palette with `@`) | Ctrl+P |
| Find an asset (the palette with `#`) | Ctrl+Shift+P, then `#` |
| Rename the picked file (after a click in the FileSystem dock) | F2 |
| Where the picked asset is used | Shift+F12 |
| The mixer (the Audio tab), open or closed | Alt+A |
| Play or stop the picked sound; pick the next file (after a click in the FileSystem dock) | Space; the arrows |
| Stop the sound playing | Escape |
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
| Pick the selected character's joints (its Skeleton), with the pointer over the viewport | Tab |
| Drag the selection over surfaces | Ctrl+Shift with a move handle |
| What rotating and scaling turn about: the middle, each entity's own origin, the active entity | . (Period) |
| Play the scene, stop ([playing](#playing)); while playing, go on from a breakpoint or a pause | F5, F8; F5 |
| Play from the editor's camera; restart | Shift+F5; Ctrl+F5 |
| Pause or go on; step one tick | F7; Shift+F7 |
| Eject (the editor's camera and tools on the running game), return | Shift+F8 |
| While playing: free the mouse from the game, give it back | Shift+F1 |
| While playing: keep the selection's changes at Stop; pick from what changed | Ctrl+K; Ctrl+Shift+K |
| In a script's tab: a breakpoint on the caret's line, or none; a logpoint ([breakpoints](scripting.md#breakpoints-logpoints-and-pause-points)) | F9; Shift+F9 |
| In a script's tab: the line's breakpoint off or on; its options; remove them all | Ctrl+F9; Alt+F9; Ctrl+Shift+F9 |
| At a script's stop: step over, into, out | F10; F11; Shift+F11 |
| The Debugger panel (the stop, its call stack and values, every breakpoint) | Ctrl+Shift+D |
| Stop at errors the script does not catch, at errors a `pcall` catches; break at the next script line | the palette's Debug commands |

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
- **Lights, cameras and sounds** show as icons in the viewport (a bulb, a spot, a sun, a
  camera, a speaker for a sound in the world, a loudspeaker for one heard as it is, an ear
  for the listener, surround waves for a reverb zone, each on a dark disk), above the
  scene: a click on one selects it, a box takes the icons in it, and the one under the
  pointer turns white (the selected ones orange). A plain click again on the icon it
  selected goes through to what lies under it, as cycling does. Empty entities have
  nothing to show: pick them in the tree.
  In Play, ejected (Shift+F8), a sound's disk is filled green while it plays and is only
  a ring while it is silent. With no listener in the scene, the game's main camera hears
  (as the player decides): its icon wears a small ear.
- **Their reach:** a selected point light shows its range as three circles, a spot light
  its cone, a camera its view's pyramid (2 m deep), the sun its direction. A selected
  sound in the world shows its two distances, each as a ring on the level plane through it
  and as its outline facing the camera: full volume inside the first (green), the
  falloff's end at the second (fainter), dashed when the sound holds its level beyond it
  (`Inverse`) and solid when it falls silent there (`Linear`). A selected reverb zone shows
  its box (violet), turned and scaled with its entity, and dashed, where its room fades
  out; a label under its icon gives its room ("Hall", or its decay when it is none of the
  presets) and its priority.
  **View: Overlays** in the palette hides icons and reach; the session keeps it.
- **View: Sound Ranges** in the palette shows every sound's outer ring and every reverb
  zone's box at once, dim, selected or not: where the level is heard, before you walk it.
  The selected ones draw their own, as above.
- **Their handles** (E11-4b): a square on each of a selected sound's rings, on the side
  toward the camera's right. Point at one to read it ("6 m · full", "40 m · −16 dB,
  held", "40 m · silent"); drag it to set the distance, by the snapping rules (G, Shift,
  Ctrl). The inner ring pushes the outer one along, and the outer one stops at the inner.
  A selected reverb zone has a square in the middle of each side that faces the camera
  (drag it: the opposite side stays, as a brush's does; its label says the size) and one
  on top of its fade (how far out its room fades). One undo step a drag; Escape takes it
  back.
- **Playing in place** (E11-4b, ADR 0124's E12-1b): **P** (Scene: Play Selection in
  Place) plays the selected sources where they are, looping, heard from the editor's camera
  as you move it; the reverb zones answer (walls and the room from the geometry need the
  game's physics, so they come with Play). It plays the selected characters too, each its
  clip where it stands (below). P again, Escape or another selection stops it, the
  characters back in their rest pose. **Sound: Listen to the Scene** plays every source set
  to play while you fly through the level; Escape stops it. Neither changes the scene:
  nothing is written back, nothing despawns, and Play ends either.
- **A character's clips** (ADR 0124's E12-1b): select an object whose model is skinned (a
  glTF mesh a skinned node draws) and its **Clips** show under its components, once its
  skin is made (a few tens of milliseconds):
  - a row a clip: its name and length; its frames, how fast its root crosses the ground
    ("in place" for a walk made on the spot), its size and its largest compression error;
    and its loop's seam: "loops within 0.2 mm", or "jumps 3.1 cm at leg_joint_R_1" in the
    warning colour when the skin jumps a centimetre or more from the last frame back to
    the first (the root's crossing and turn taken out, as root motion will take them);
  - a click on a row shows its first frame; ▶ plays it in place from the playhead, ■
    stops it there and holds that pose; the loop, the speed (0.25 to 2×) and the time,
    and a bar under them that scrubs it. Escape lets a held pose go.
  - After a click in the Clips, **↓** and **↑** move to the next and the previous clip,
    as a click on its row would. With Auto Play on, a picked file's clip plays at once.
  - **Its `Animator`** (Add Component: Animator) plays a clip in Play. Its `clip` field is
    a list of the character's clips (empty: its first clip, which is what plays). While you edit, the character stands at the animator's `time`.
  - **Its skeleton:** while it is selected, its joints show as thin white sticks in front
    of its mesh, one from each joint to its parent, moving with the clip it plays.
    **View: Skeletons** shows every character's too, in grey.
  - **Its joints:** its **Skeleton** section lists them as a tree, each under its parent.
    **Tab** with the pointer over the viewport shows a small square at each joint. The
    one under the pointer lights up with its name; a click picks it (orange, its name in
    the status bar), and the selection stays the character. A click on a row of the
    Skeleton picks that joint too. Tab, Escape, or a click in the view on no joint ends
    it.
  - **Attach something to a joint:** with its joints showing (Tab), drag a row of the
    scene tree onto a joint's square in the view or onto its row in the Skeleton. The
    row is dragged alone and the character stays selected. On release the thing goes
    into the joint's socket, a `JointAttachment` named after the joint, made right under
    the character where it has none. It keeps its place in the world (Alt keeps its
    local placement) and follows the joint from then on. One undo step.
  - Pick a skinned model's file in the FileSystem dock and its Clips show the same way,
    under the engine's own drawing of it (drag it to turn it): ▶ plays the chosen clip
    there, and with the dock's **Auto Play** on its first clip plays as soon as its clips
    are read. Nothing of the scene changes.
    - It stands on a floor of the blockout's grid. A clip that crosses the ground walks on
      its spot while the floor goes by under it, back by the root's motion and turned by
      its turn, so you see how far and which way it goes (Godot's root motion view). A
      clip made in place leaves the floor still.
    - **In Place**, beside the speed, holds the floor still too: the walk is seen on a
      treadmill. Choosing another clip puts the floor back.
    - **Ctrl+click** another clip's row and it plays beside the chosen one, at the same
      fraction of its length: compare two takes, or check that a walk and a run plant the
      same foot first. Its row is marked in a paler colour. Ctrl+click it again to take it
      away; choosing it makes it the chosen one.
  - **The Animation tab** (Alt+N, or the bottom bar) shows the same chosen clip on a
    timeline. It works for the selected character, or for the model file picked in the
    dock.
    - **Its header:** ↑ and ↓ step to the clip before or after; then whose clips they are
      and the clip, ▶ or ■, the loop, the speed, and where the playhead is ("frame 165 /
      328 · 1.72 s").
    - **Its ruler** numbers the frames as far apart as they read. A press or a drag on it
      puts the playhead there and holds the pose.
    - **Its row** names the character with its joints and the clip's sample rate.
    - After a click in the tab, **←** and **→** step one frame (the clip stops there),
      **Home** and **End** go to the first and last frames, and **Space** plays or stops.
    - The tab and the Clips share one playhead: what one shows, the other does.
    - **Events** (footsteps and other moments, see the
      [animation guide](animation.md#clip-events-footsteps-and-other-moments)) sit on the
      strip under the ruler, a mark and a name each.
      - **E** (after a click in the tab) or **+** adds one at the playhead's frame, named
        "event", its name ready to type: type and press Enter.
      - A click on a mark picks it. Drag it to move it, frame by frame.
      - The picked event's **name**, **data** and **sound** (a path in the project) are
        typed in the row under the strip. **Delete** or its **✕** removes it.
      - Every change is written to the model's `.meta` at once and is one step of undo.
        A game playing in the editor hears it from its next tick.
      - **Their sounds play in the previews:** the dock's model playing its clip, and P
        playing a character in place, play an event's sound as they pass it, on the same
        bus as the dock's sound previews (one at a time).
    - **Pops, found:** pick one of a character's joints (Tab in the view, or its row in
      the inspector's Skeleton) while the tab shows his clips.
      - The joint leaves a **trail** through the clip in the view: its path in magenta, a
        tick at each frame.
      - Its **speed** draws under the events, a bar a frame. A pop is a bar far taller than
        its neighbours; the tallest is lit. The row beside names the longest step, its
        frames and its speed ("leg_joint_L_2: the longest step 5.2 cm, frame 4 to 5").
      - A click or a drag on the bars puts the playhead there.
    - **Mark Foot Plants** (the tab's button, or the command palette) puts a `footstep`
      event, data `left` or `right`, where each foot touches down in the chosen clip, in
      place of the footsteps it marked before; one step of undo.
      - The feet are the joints named for a foot (`foot`, else `ankle`). Without those, it
        takes the lowest joint of each side the names give (`leg_joint_L_5`). The status
        bar names the feet it used.
      - A touch-down is where the foot comes down into the lowest fifth of its height in
        the clip; a loop's footstep across its end is marked once.
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

**View: Colliders** (the palette, or Alt+C as in Unreal) draws what collides as thin lines
in the world: the edges of the brushes' pieces ([blockout.md](blockout.md#colliders)) and
every model's collider, made from its mesh by the rules beside its file as the cook makes
it ([physics.md](physics.md#models-in-scene-files)), and every `Trigger`. The color says
how a body moves: cyan static, orange dynamic, violet kinematic, yellow a sensor or a
trigger. Lines are bright where they
are in front, faint behind a surface, and never flicker on the surface they lie on
(ADR 0116, ADR 0117). A model's collider is made in the background the first time it is
shown, and again when its file is written anew.

### Navigation

**View: Navigation** (the palette) bakes the navigation mesh of the brushes that collide
and draws it in the world, just above the surface: its boundaries (walls, drops) bright
green, the edges between its polygons faint, both fainter behind a surface, for a
human-sized agent (1.8 m tall, 0.4 m wide-radius, 0.4 m steps, 45° slopes;
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

## View modes

The chip beside "Perspective" at the viewport's top left shows how the view shades the scene;
a click opens its menu (ADR 0116). Unreal's keys switch it too:

| Mode | Key | What it shows |
|---|---|---|
| Lit | Alt+4 | the scene as the game draws it |
| Unlit | Alt+3 | each material's base color and emission, no light, over a dark background |
| Wireframe | Alt+2 | every triangle's edges over a dark background, those behind others too |
| Lit + Wireframe | Alt+5 | the lit scene with the edges of what is in front |

- The edges are each mesh's finest level. They are drawn after the frame at the view's own
  size: an edge on a surface never flickers against it, and an edge behind another
  surface is left out (Lit + Wireframe).
- Unlit reads a custom material type as its glTF fallback.
- The session keeps the mode.
- Alt+C draws the blockout's colliders over any mode ([Colliders](#colliders)); the models'
  colliders and the bodies in Play come next.

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
  tweens one tick on, then paused again. At a script's stop it finishes the stopped tick
  instead, its stops passed, and pauses after it ([scripting.md](scripting.md)).
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
  window free it as well; going on and returning give it back. While it is free the game
  does not hear it (its buttons, motion and wheel), but it still hears the keyboard.
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
  - a material is drawn on a sphere; a material type's once its shaders are compiled, not
    its look;
  - a sound is its waveform over its whole length, every channel together; a part that
    reaches full scale (it may clip) shows in orange;
  - other kinds show their icon.
- **Sizing:** Ctrl+wheel over the tiles makes them larger or smaller.
- **Listening to sounds:** ▶ over a sound's tile plays it, ■ stops it.
  - After a click in the tiles, Space plays or stops the picked sound, and the arrows move
    the pick: Up and Down a row, Left and Right one file.
  - **Auto Play**, at the right of the tiles' header, plays each sound as it is picked (a
    file that is not a sound stops it). It is kept with the session.
  - While a sound plays, its tile is tinted up to where it is, with a line at that point;
    a click along the tile plays from there. Escape stops it.
  - A sound plays as the file is, on a bus of the editor's own straight to the speakers:
    never through your game's buses, their effects or a bus you muted, and heard while
    nothing plays. One at a time: a new one stops the last.
  - A long file streams from where you ask, read in the background, so the editor never
    waits for it.
  - The palette has both: "Assets: Play or Stop the Picked Sound" and "Assets: Auto Play
    Sounds".
- **Renaming:** F2 after a click in the tiles renames the picked file in place (its
  extension is kept when you leave it out). A file dropped on a folder, a tile or a row
  of the folder tree, moves into it.
  - The `.meta` goes with the file, so it keeps its id and every scene keeps its
    references.
  - A scene saved after it writes the new path.
  - **A sound** is named by its path under the sounds folder, so what names it is
    rewritten: the sources in the open scene, in the other tabs' scenes, and in the
    project's scenes and prefabs on disk, and the names undo would put back. Scripts are
    not changed (a string there can be anything): when one names the old path, the
    palette lists its lines (`%`) for you to change by hand. A sound moved out of the
    sounds folder cannot be followed: the message counts the sources left naming it.
  - Neither is an undo step: they are files on disk.
- **Find References** (Shift+F12) lists where the picked asset is used, in the palette:
  - the open scene's entities, as the scene is now (Enter selects one);
  - then the project's scenes and materials on disk, with how many times (Enter shows the
    file);
  - for a sound, the scripts that name it in their text too (Enter opens one).
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
- **A sound** picked in the dock shows its waveform under **Sound**, a band per channel
  (the left above the right), read in the background the moment it is picked:
  - ▶ plays it from its start and ■ stops it; the loop button plays it over and over;
  - a click on the waveform plays from there; a drag moves the playhead and the time
    with it, and plays from where you let go;
  - the time is where it plays and its length: "0:01.20 / 0:03.20";
  - silence at its start and its end is shaded, and the rows below say how long it is,
    its rate and channels, its **peak** in dBFS ("(it may clip)" at full scale) and its
    **silence** at each end ("0.18 s first");
  - it plays as the dock's previews do: on the editor's own bus, never through the game's.
- **A material** (`.material`) picked in the dock shows its own rows under **Material**:
  - the standard material's base color, metallic, roughness, emissive, alpha (with its
    cutoff when masked) and double sided;
  - a material type's instance: the type and its module, then a row for each field of its
    `Params` struct, read from Slang's reflection: numbers, a vector's parts, an image
    (drop one from the dock onto its row) with its encoding; then its look, and the look's
    values when it is its own.

  An edit shows in the scene by the next frame, and nothing compiles: drag a number, type
  it, pick a choice, or ↺ to put a value back. Every edit is an undo step in the one
  history (a drag one step). The section reads "Material (unsaved: Ctrl+S)" until **Ctrl+S**
  writes the file with the scene; unsaved edits are kept with the session, as a script's
  are, and come back with the editor.
- **From the object:** a selected object whose model has a material file shows that
  material's rows after its components, with how many of the scene's objects use it
  ("Material · used by 40 objects"): an edit there changes all of them.
- **Material commands** (the palette):
  - **New Material** makes `New Material.material` in the dock's folder and gives it to the
    selected objects in one undo step (picked in the dock when none is selected);
  - **Make Variant** makes a variant of the material shown beside its file (`Red 2.material`,
    an id of its own) and gives it to the selected objects in one undo step. A variant names
    its **base** and holds only what it changes: it draws as its base until edited, and an
    edit of the base (saved, or unsaved in the inspector) reaches every variant where the
    variant has not set its own value. A variant of a variant is fine; a chain that comes
    back to a file of it is refused. The cook writes variants out whole, so a game never
    reads a chain;
  - **a variant in the inspector:** its title says whose variant it is ("Material ·
    variant of red.material"), and its first row is its **base** (a click shows it in the
    dock). A row whose value is the variant's own has ↺, which puts the base's value back;
    a row without ↺ follows the base;
  - **Apply to Base** writes the variant's own values into its base, so every variant of
    that base gets them, and leaves the variant naming its base alone, in one undo step;
    Ctrl+S writes both files;
  - **its preview** at the top of its section: the material drawn by the engine on the
    selected object's own mesh (a sphere when it is picked in the dock), redrawn as you edit
    it; drag the picture to turn it;
  - **what a material type costs** your GPU: once its shading is made, its **cost** row
    (after its type) gives the fragment shader's registers over the shading variants made
    (plain, with local lights, with the environment, with both), the engine's own shading's
    beside them, what spills to memory, and its parameters' bytes: `39 registers (the
    engine's 47) · 48 B of parameters`. Registers come first: on an integrated GPU they
    decide how many pixels shade at once. A graph's bar says the same of its preview (the
    editing build, its numbers parameters until saved);
  - **its code** row, after the cost: the instructions and image samples its fragment
    shader reaches, beside the engine's own shading's, and how long its last compile took:
    `805 instructions (the engine's 936), 3 samples (the engine's 8) · compiled in 0.40 s,
    1 entry point`. These count the code, not what runs: a loop runs its body more often,
    a branch less;
  - **draw as fallback** (the checkbox after them, or Material: Draw as Fallback in the
    palette) draws the material with its look, the plain material its type falls back to,
    so the profiler's forward pass shows what the type's shading costs over the engine's
    own, in this view on this GPU. It is the editor's, not the file's (nothing to undo),
    and the section's title says "drawn as its fallback" until you turn it off;
  - **Apply to Selection** gives the material picked in the dock to the selected objects;
  - **New Type** writes your own material type from a template, named after the active
    object (`Water`: `water.slang` declaring `WaterMaterial`; `Custom` with none), and a
    material file of it beside it (`water.material`: a grey color, roughness 0.5, not
    metallic), both in the dock's folder. The file goes to the selected objects in one undo
    step, and the module opens in a code tab at its surface, the line to write in. Until
    its shaders are made (1-2 s the first time) the objects draw the file's look.
    Each Ctrl+S of the module compiles it again, the previous version drawing meanwhile and
    on an error; a field added to its parameters becomes a row of the material once it has
    compiled, the other values kept by name.

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
| a material (`.material`) | an object in the viewport, or its row | the object shows it while the pointer is on it; letting go gives it to the object's model |
| a sound (`.wav`, `.ogg`, `.flac`) | an object in the viewport, or its row | it plays the sound in the world: an `AudioSource3D`, or its source's sound replaced |
| a sound | the viewport where no object is (or anywhere with Alt) | a new entity named after the file, on the surface under the pointer, playing it in the world |
| a sound | the tree below its rows | a new entity playing it as it is (an `AudioSource`: music, ambience, the UI) |
| an asset | an asset field in the inspector | the field takes it, on every selected entity with that component |

- **Placing in the viewport:**
  - the model's box rests on the surface: its bottom on a floor, its side on a wall;
  - where nothing is under the pointer, it stands on the ground's plane (y = 0);
  - with snapping on, its place along the surface snaps to the position step (Shift
    inverts it, as for drags).
- **Escape**, or moving back out of the viewport, takes the model away again. Every drop
  is one undo step.
- **A material shows before you let go** on the object under the pointer (or the row's
  object), nothing recorded yet. Moving onto another object, off it, out of the view, or
  Escape gives the object its own material back; letting go gives it as one undo step from
  its own, so Ctrl+Z puts the own one back. The object stays the one under the pointer as
  it was, so a see-through material (masked or blended) shows on it steadily as the
  pointer moves instead of letting the pointer through to what is behind.
- **What a model becomes:**
  - a file of one node is one entity, named after the file;
  - a character (a file whose mesh is drawn skinned) is one entity too, named after the
    file and turned and scaled as its file stands it. Its skeleton's joints, and nodes
    that draw nothing, become no entities (ADR 0124). The file's other meshes become
    entities under it, at their places in the file. A mesh under a joint (a sword in a
    hand) comes in a socket on that joint, so it follows the hand as the character
    plays;
  - otherwise a root named after the file, with an entity for each node of the file's
    scene (its transform, and a model of its mesh and material).
- **Fields:** a model's `mesh` takes a model file of one mesh, its `material` a material
  file or a model file of one material.
- **Sounds** must be under the sounds folder (`AudioSettings::folder`, `sounds/`): a
  source names its sound by its path there (`ambience/hum.wav`); a file elsewhere is
  refused, and the hint says why.

**Asset fields in the inspector** show the asset as Godot's do:
- a square preview the engine draws: a mesh with its own material, a material on a
  sphere, an image as itself;
- the asset's name beside it.

A click opens the palette on the project's assets of the field's kind (`#model:`,
`#material:`), and Enter puts the one picked in the field, on every selected entity. A
file dropped on the field takes its place.
- **A sound field** (a source's `sound`) shows the file it names with its waveform, and ▶
  at its end: ▶ plays it as the dock's previews do (■ stops it). A click opens the palette
  on the sounds (`#sound:`); with Auto Play on, each file the arrows reach plays, Enter
  gives it to the field and Escape stops it. Only sounds are taken, from the sounds
  folder.
- **A bus field** (a source's `bus`) is a list of `master` and the mixer's buses, as its
  strips stand. A name the mixer has no bus for is shown in red after ⚠: the sound plays
  on the master.
- Your own components' strings can be such fields too: `#[reflect(hint = "sound")]` or
  `#[reflect(hint = "bus")]` on the field.
- **A sound in the world shows its falloff** after its components: its level over
  distance as bars, from its place (0 m) out to its outer ring, half again past it when
  the sound holds its level there (`Inverse`); a bar's height runs from −48 dB (empty)
  to full. The bar at the editor camera's distance is red, and the line above says what
  is heard there ("−11 dB at 22.0 m", "full at 3.0 m", "silent at 45.0 m"). Drag a
  ring's handle or type a distance and the bars follow; fly the camera and the red bar
  moves.
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
  - a file and line of your game's code (`src/main.rs:12:5`) opens in VS Code; a
    script's or a Slang module's (`scripts/door.luau:7`) opens in the editor's code tab.

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
  count;
- while the game plays, its costliest scripts: each script component and system with its
  hooks' time and calls a frame and the memory it holds, over the last half second
  ([scripting.md](scripting.md#measuring-your-scripts)). They are measured only while the
  panel is open.

For a whole timeline, run under Tracy with the profiling build ([profiling.md](profiling.md)).

## The mixer

The bottom bar's **Audio** tab (Alt+A, "View: Audio Mixer") is your game's mix: the
project's `main.mixer` ([audio.md](audio.md), ADR 0121), or the built-in buses until you
change something.

- **A strip per bus**, the master first, then each bus after the bus it feeds (a dot for
  each step down): its name, where it goes, its fader and its gain in dB.
- **The fader:** drag its thumb; there is more travel near 0 dB, as on a console, with
  marks at 0, −6, −12, −24 and −48. Hold Ctrl for whole dB. The game hears it at once,
  playing or not. A drag is one undo step, and Escape takes it back.
- **A click on a strip** shows its bus in the inspector: its **name** (a rename carries the
  buses it feeds along, and the open scene's sources on the bus in the same undo step;
  the message names the other scenes still on the old name, which play on the master
  until changed), its **output** (the list leaves out the buses that would make the
  sound go round), its **gain**, **balance** and **voices** (the most mixed at once, 0 for
  no limit), dragged or typed as every number is, and **reverb** (its 3D sounds take on
  the room).
- **+ Bus** ("Audio: Add Bus") adds one into the master, picked to be named; a picked
  strip's × takes it away, and the buses it was fed by go to the bus it fed.
- **Snapshots** (ADR 0122; your game blends them with `mixer.blend` or
  `audio.blend`): the list at the header's right, "Mix" first.
  - **+ Snapshot** (or "Audio: New Snapshot") makes one and chooses it. While a snapshot
    is chosen, the panel shows it over the mix and the player hears it alone at full
    weight, in the editor and in Play; "Mix" gives the game's own weights back.
  - **Its values:** a fader dragged, a balance, a send's level, an effect's parameter or
    bypass set while it is chosen becomes one of its values (one undo step each); the
    mix stays as it was. What it sets is marked: the fader's number in the accent color,
    the inspector's field with ↺, which takes the value out of the snapshot.
  - **Buses, effects and sends themselves** are the mix's: adding, removing, renaming or
    moving them while a snapshot is chosen is refused with a message; choose Mix first.
  - **Picked strips show a Snapshot section** first: its name (rename it there), its fade
    in seconds (what a blend takes when the game names none) and how many values it sets.
    "Audio: Delete Snapshot" takes the chosen one away.
- **The room** ("room (auto)", at the right end): the reverb the engine makes from the
  room around the listener. Its strip says what it follows ("Hall 62 %": a reverb zone
  and how far into it the listener is; "the walls"; "nothing near"), and its meter is the
  reverb's. Its fader is a trim kept in `main.mixer` (`room.gain`); everything else
  follows the room. Picked, the inspector shows the trim and, read-only, the room heard
  now: what it follows, decay, size, damping, pre-delay and level. A soloed bus keeps its
  reverb: the room is never silenced by a solo.
- **The meters:** a bar a channel beside each fader, −60 to +6 dBFS, falling back at
  24 dB a second, yellow from −6 dBFS and red over full scale. Above them, the loudest
  peak since you last looked, and **!** when the bus went over full scale (it may clip).
  A click there clears them; Ctrl+click clears every strip's. Only the Audio tab reads
  them, so a game costs nothing for them.
- **Listening:** **M** mutes a bus and **S** solos it: while a bus is soloed, you hear it,
  the buses it feeds on the way to the master, and the buses that feed it; the others are
  silent and dimmed. **Monitor** sets every solo and mute aside, and back. They are for
  your ears only: kept with the session, never in `main.mixer`, and they leave the game's
  own mutes (`Mixer::set_muted`) as they are.
- Every change is an undo step ("Mixer: music −6.0 dB"), and Ctrl+S writes `main.mixer`
  with the scene. The header says when it is unsaved, or not written yet. Unsaved changes
  are kept in `.ofelia/` with the session, as a scene's are, and come back at the next
  start.
- **Effects and sends** (ADR 0121's version 2): pick a strip, and the inspector shows a
  section for each of its effects after the Bus section, in the order the sound goes
  through them:
  - **Add:** "add effect" lists the twelve built-in kinds (the filters, compressor,
    limiter, delay, reverb), "add send" the buses it can send into without looping the
    sound back;
  - **an effect's section:** its parameters in their units (drag them; a frequency moves
    an octave per 120 px; click to type, expressions too), bypass, a filter's shape, and a
    compressor's sidechain (only the buses that would not loop); ↑ and ↓ move it in the
    chain, × takes it away; a compressor's or a limiter's title says how far it is lowering
    the gain now;
  - **a send's section:** its level in dB and whether it is taken before the fader;
  - **on the strip:** the effect's short name ("Comp"), or how many ("3 fx"), with the
    most gain reduction now, and **B** to bypass them all (again to bring them back); a
    send shows as `+1` after the output;
  - renaming or removing a bus carries its sends and sidechains along, or takes them away;
  - every change is a step of the one history, heard at once.
- **While the game plays,** mix it: every change is heard at once, and a dot marks each
  strip changed since Play (the header counts them). The changes stay after Stop, as one
  undo step, "Mixer: changes in Play"; undo it, or run "Audio: Take Back Play's Mixer
  Changes", to have the mixer as Play found it.
- A `main.mixer` changed outside the editor is read again, unless you have changes of
  your own in it.

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

Scripts open in tabs after the scenes' (ADR 0107 E8), their code where the viewport is.
A code tab has a panel of its own on the left, and the scene's tree and inspector step
aside (**View: Scene Docks Beside Code** keeps them):

- **Open Files:** your open scripts, Slang modules and text files. A dot marks unsaved
  ones; the count is what Luau or Slang found. Click one to show it, its close mark to
  close it, and type in the filter to narrow the list.
- **Outline:** the file's functions, components and types (a text file's sections and
  headings). The one the caret is in is lit; click one to go there.

Tabs:

- **Opening one:**
  - a double click on a `.luau` file (or a `.slang` module) in the FileSystem dock;
  - a double click on a text file there: TOML, RON, JSON, Fluent (`.ftl`), Markdown,
    plain text. They are colored as in VS Code; nothing checks them, and they get no
    `.meta`;
  - "File: Open Script…" in the palette;
  - a click on an error at a script's line in the Output, which opens it at that line.
- **A scene's tab** brings the scene back in front; Ctrl+Tab goes through the scenes'
  tabs, then the scripts', then the material graphs'.
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

Luau is highlighted as VS Code's luau-lsp extension colors it (keywords, strings,
comments, numbers, function names, `self` and the standard library, types).

**Finding and replacing** work as in VS Code:

- **Ctrl+F** finds as you type, and lights every match. Enter goes to the next one,
  Shift+Enter (or F3 and Shift+F3) the one before.
- **Alt+C** matches case and **Alt+W** whole words.
- **Alt+Enter** puts a caret on every match.
- **Ctrl+H** adds the replace field. Enter replaces the match and goes on; Ctrl+Alt+Enter
  replaces them all, and Ctrl+Z takes them back at once.
- **Escape** closes the bar.
- **Ctrl+G**, then a line number (`:12`, or `:12:5` for a column), goes there.

**Luau checks your script as you type,** with Luau's own type checker and linter, the
ones `luau-analyze` and VS Code's luau-lsp use:

- a mistake is underlined, its message after the line;
- the code bar counts errors and warnings, and a click there goes to the next one;
- `--!strict` at the top checks every type.

**Completion** opens after `.` and `:`, at a word's first letter, and on Ctrl+Space:

- Up and Down pick an item, and Enter or Tab writes it;
- a function gets its parentheses;
- Escape closes the list.

**Rest the mouse on a name** for half a second to see its type.

- **F12** (or Ctrl+click) goes to where a name is defined, in another script too.
- **Ctrl+Shift+F** finds text in every script of the project. The palette lists each line
  that has it; Enter opens one there, the match selected.
- **Inside a call's parentheses**, its signature shows above the line, the argument you
  are at lit. Ctrl+Shift+Space shows it, Escape hides it.

**Breakpoints** sit in the column left of the line numbers, in the game's scripts: ● stops
the script at its line during Play (F5 goes on, F8 stops Play there), ◆ logpoints write a
message and the script goes on, ‖ pause points pause Play after the tick. A click or F9
sets one, Shift+click or Shift+F9 a logpoint, a right click or Alt+F9 opens its options;
[Scripting in Luau](scripting.md#breakpoints-logpoints-and-pause-points) has the rest.
At a stop the script's tab comes to the front with the arrow on the line and the frame's
values after it, the viewport's edge turns amber, and the editor works as while paused;
F10, F11 and Shift+F11 step (over, into, out). The Debugger panel opens with it: the call
stack, the shown frame's values and every breakpoint. An error a script does not catch
stops it the same way, at the failing line, in red with its message (once a line a Play;
the Breakpoints list's first rows choose which errors stop). At a stop the lines above
show what they assigned, the Variables open into tables, components and entities, and the
Evaluate line under them evaluates Luau in the shown frame (Ctrl+Enter keeps it as a
watch).

**Slang modules** (`.slang`) open in code tabs too, with Slang's colors:

- **Slang checks them as you type,** with the compiler the engine uses.
  - A material type (`struct Water : IMaterial`) is checked as it compiles, inside the
    engine's `forward.slang`. `IMaterial`, `MaterialContext` and `Surface` are known.
  - A mistake is underlined at its place about a quarter of a second after you stop
    typing, counted in the code bar.
  - An error in a file the module includes shows on its first line, with that file and
    line.
- **Completion, hover, signatures and F12** come from Slang's language server, `slangd`,
  which ships with Slang beside its library:
  - in a material type they know `forward.slang`'s types: `ctx.` lists a
    `MaterialContext`'s fields, and F12 on `MaterialContext` opens `forward.slang` there;
  - resting the mouse on a name shows its declaration and its documentation;
  - without `slangd`, Slang's tabs only check, and the Output says so once.
- **Ctrl+Shift+F** finds text in the Slang modules too.

## Material graphs

A material graph (a `.mgraph` file, ADR 0123) opens in a tab of its own after the scripts',
its canvas where the viewport is: a double click on it in the FileSystem dock. The bar over
the canvas names the file, its nodes, the selection and the zoom.

**Material: New Graph** (Ctrl+Shift+P) starts one from the selected object. It writes
`<name>.mgraph` and `<name>.material`, named after the object, in the dock's folder, gives
the material to the selected models in one step, and opens the graph:

- **For a glTF model's material**, the graph draws that material's own look:
  - its colour map times its tint;
  - its metallic and roughness map's channels (blue and green) times their factors;
  - its normal map through a Normal Map node;
  - its emission and its occlusion.

  A map is used only where its image is a file in the project (an image inside a `.glb`
  is not). The metallic/roughness and occlusion maps are marked as data in the material
  file, not colours.
- **For anything else**, the graph draws the standard material: a grey colour, roughness
  0.5, no metal.

Until its shaders are made, the material draws its colour.

- **The canvas:**
  - each node is a box: its title (a parameter's name after its kind's, `Number ·
    roughness`), its inputs down the left edge and its outputs down the right. A colour
    shows its swatch, a number its value;
  - the title bar's colour is the node's group (inputs teal, maths blue, vectors purple,
    the surface red); a port's colour is what it carries (a number grey, two components
    green, three yellow, four pink, as wide as what is linked blue). The ports' names say
    it too;
  - the surface shows the inputs its shading model reads, and any other that is linked.
- **Adding nodes:** **Space** (or Blender's Shift+A) over the canvas opens the node search
  at the pointer:
  - every kind by its group (`Math: Multiply`), its ports beside it (`a, b → out`);
  - type to narrow it, the best matches first, then Enter or a click adds the node where
    the pointer was, selected;
  - **Graph: Add Node…** in the palette does the same, at the canvas's middle.
- **The mouse:**
  - **drag from an output to an input** (or from an input to an output): a link. A link
    dropped on an input replaces the one it had. A link that would feed a node from
    itself is refused, and the status bar says why;
  - **a link dropped on the empty canvas** opens the node search there. It offers only the
    kinds that can take the link, and the node picked is linked by its port that fits best
    (one as wide, else a dynamic one);
  - **an image dragged from the FileSystem dock** onto the canvas is a texture node there,
    its parameter named after the file (`Wood Grain.png`: `wood_grain`);
  - **drag a linked input's end**: its link comes off; drop it on another input to move
    it, or on the canvas to take it out;
  - **click a box** to select it, Shift+click to add it or take it out, and drag it (the
    selection moves with it, to whole units);
  - **drag on the empty canvas**: a box selects what it meets (Shift adds); a click there
    lets the selection go;
  - **Ctrl+drag from an output** carries all its links: drop them on another output to
    move them there, or on the canvas to take them out (a move that would make a loop is
    refused, all of them kept);
  - **the middle or the right button** pans, **the wheel** zooms about the pointer (25 %
    to 300 %, by steps).
- **The inspector** shows the node selected alone, or the graph with none:
  - **the graph:** its shading model (lit, unlit, clearcoat, cloth, hair, toon, sheen,
    sheen with clear coat), which picks the surface's inputs;
  - **a node:**
    - its parameter's name: the material's row by that name; empty, the value is compiled
      in. A name is letters, digits and `_`, and no other node's;
    - a constant's value (a colour by its channels);
    - its unlinked inputs' own values (one number for an input as wide as what is linked);
    - a texture's image: a click picks one, or drop an image from the dock onto it;
    - its note.
  - Numbers are dragged sideways or typed (a click; expressions as in other fields).
    Backspace over a row, or its ↺, resets it. Each edit is one step, a drag's moves one.
- **The keys:** Delete deletes the selected nodes (the surface stays), Ctrl+D copies them
  (each with an id and a parameter name of its own, 30 units down and right, with the
  links into them; the copies selected), F frames them (all of them, none selected),
  Escape takes a drag back or lets the selection go, Ctrl+S
  writes the file, Ctrl+W closes the tab (asked twice with unsaved changes), Ctrl+Z and
  Ctrl+Y undo and redo. The scene's keys (the tools' letters, H, B) do nothing while a
  graph is shown.
- **Every edit is one step** of the editor's one history: a link, a move, a copy, a
  delete. A
  step of a graph whose tab was closed, undone, opens it again (behind the scene, unsaved).
- **Saving** writes the file; the materials that name the graph draw it again (ADR 0123
  E10-3c). Unsaved graphs are autosaved as scenes are, and the session brings the tabs
  back, with their unsaved edits.
- **The preview:** a sphere in the canvas's top right corner draws the material as the
  graph is now, unsaved edits and all.
  - A number dragged or typed reaches it within a few frames: the preview's build makes
    every number a parameter, so nothing is compiled.
  - A link or a node that changes the code is compiled first (about half a second).
  - When the graph cannot be drawn, its bar says why ("not drawn: …").
- **Node previews:** under its ports, each node shows its value, as the material computes
  it at that node, drawn by the engine from the preview's build (ADR 0123 E10-4g).
  - The tile is a flat square of texture coordinates (0 to 1 across, the first at the top
    left). It is a sphere where the node reads the surface's place, normal or tangent, or
    may (an Expression, or a `[Node]` function given the fragment), as Unity's previews
    are.
  - The value is shown as it is: what is below 0 is black, what is above 1 is white.
  - Numbers reach the tiles as they reach the sphere, within a few frames; a change of code
    is compiled first, with the sphere's. A dark square stands in until a tile is drawn.
  - **Graph: Node Previews** (the palette) turns them off and on, and the boxes shorten.
    The choice is kept in the session.
  - A function call shows what its first output gives. In a function's own tab, every node
    shows its value as the function's preview computes it. An input shows the value it
    gives where nothing is linked to it; an output shows what it is given.
  - The surface and reroutes have none.
- **Errors on nodes:** a node that stops the graph from generating is framed in red, with
  why under it: a loop, or a kind or a port that is not there (a file merged from another
  branch, or written by a newer engine). It goes as soon as its cause does.
- **Show Slang** (the graph bar's button, or the palette's "Graph: Show Slang") opens the
  code a game draws the graph with, the build that is saved, in a code tab of its own.
  - The tab is read only, named after the graph (`glow.mgraph.slang (read only)`):
    typing is refused and Ctrl+S writes nothing.
  - It follows the graph while the graph's tab is open: edit the graph and look again.
  - It is not kept in the session.
- **Reroutes:** double-click a link to put a reroute on it, a point the link passes
  through, to lead it round other nodes; or add one from the node search ("Reroute").
  - Drag its middle to move it; its ends are its pins, as a node's ports are.
  - It is drawn in the colour of what passes through it, and it changes no code.
- **Frames:** select nodes and press **C** to put them in a frame, a titled area behind
  them (Unreal's comment boxes).
  - Drag its title bar to move it with the nodes it holds: a node is in a frame while its
    box lies wholly inside it, so dragging a node in or out puts it in or takes it out.
  - Drag its bottom right corner to resize it.
  - Click its title bar to select it; its title is in the inspector. Delete removes the
    frame and leaves its nodes.
  - A frame is layout only: it changes no code. Files with frames are the format's
    version 2; older files read as they are.
- **The Expression node** (the node search's "Expression", under Code): Slang written in
  the node, as Godot's Expression node.
  - Its code is the inspector's Code row. It is either an expression of the inputs `a` to
    `d` and of the fragment's `ctx` (`ctx.uv`, `ctx.world_position`, `ctx.normal`), such
    as `a * 0.5 + sin(b) * 0.25`, or lines that return one, such as
    `float4 x = a; return x * 2.0;`. Empty, it is `a`.
  - Each input is four numbers; what is linked to one is widened or narrowed as anywhere
    else. Its output is four numbers.
  - Its title shows the code's first line.
- **Slang's errors at their nodes:** code that Slang does not take frames its node in red
  with Slang's message under it. The bar says "not drawn: node …", and the preview is not
  drawn until the code is fixed.
  - The frame stays while the code does: moving a node or typing a value keeps it.
  - Typing the code right takes it away and draws the preview.
- **Slang functions as nodes:** mark a function of one of the project's Slang modules
  with `[Node("Title", "Group")]`, the module including the mark's declaration:

  ```slang
  #include "nodes.slang"

  [Node("Swirl", "Mapping")]
  float3 swirl(MaterialContext ctx, float3 color, float amount, out float mask)
  {
      ...
  }
  ```

  - The node search lists it under its group ("Mapping: Swirl") with its ports. The editor
    reads the project's modules a moment after a graph is first shown, and reads a module
    again whenever its file is saved.
  - Its inputs are its parameters; its outputs are what it returns (`out`) and its `out`
    parameters. An `inout` parameter is both. A first `MaterialContext` is the fragment,
    not an input.
  - Every port is `float` to `float4`. A function with other types is listed with why, and
    cannot be added.
  - The node is titled by the mark. The graph keeps the function's signature, so the cook
    and the game need no Slang to read it. Change the function and its calls follow (the
    graph's tab is marked changed); a call whose function is gone is an error at its node.
  - Slang's errors in the module are shown at its first call.
- **Functions** (`.mfunc` files): a group of nodes made one node of its own and called from
  any graph, as Unreal's material functions and Blender's node groups are.
  - **Make one:** select nodes and press **Ctrl+G**. They go into `New Function.mfunc`
    beside the graph, and a call of it takes their place, linked as they were. Rename the
    file in the FileSystem dock: its calls take the new name.
    - The function's inputs are what the nodes read from outside: one per output read,
      named after the port that reads it. Its outputs are what is read of them outside.
    - Ctrl+Z takes the call back; the file stays.
  - **Open one:** double-click it in the dock, or select a call and press **Tab**. Tab
    again with no call selected goes back to the graph you came from.
    - A function's tab is a graph like any other, without a surface. Its **ports** are its
      input and output nodes (the node search's Function group, `float` to `float4`). Each
      is named by its Name row, and they are ordered as they sit on the canvas, top to
      bottom.
    - An input's Value is what a call gives where nothing is linked to it, and what the
      function's own preview shows.
    - The preview sphere shows the first output, unlit.
  - **Call one:** the node search lists the project's functions under Functions, with
    their ports. A call keeps its function's ports, so a graph reads without the file.
    Edit the function, even unsaved in its tab, and its calls follow, as do the previews of
    the graphs that call it.
  - **Take a call apart:** select it and press **Ctrl+Alt+G**. The function's nodes go
    where the call was, under their own names where those are free, linked as the call
    was. A function that the function calls stays a call.
  - Before a graph is written, each call is replaced by its function's nodes (inlining,
    as Unreal does). An error inside a function is shown at its call ("in tint: …").

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
| E7 | Luau: props of text, prefabs from a release's packs, script components in the network schema (the rest of v1 is done, ADR 0114) |
| E8 | the code editor: rename and references across the project (everything else is done) |
| E12 | animation: a character's joints in the viewport and the tree, and attachments (E12-1c), ↑/↓ through clips, two side by side and the floor under root motion (E12-1b-3), the `Animator` (E12-2), the Animation tab (E12-3), keys (E12-4) and graphs (E12-5), as ADR 0124 decided |

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
