# Viewport tools: selection, gizmos, snapping, placement

Checked: 2026-10-17. Products were studied as products (behaviour, documentation); Godot
and Blender's manuals were read, no code was carried over. Navigation is in
`viewport-navigation.md`. Input for [ADR 0107](../../adr/0107-editor.md).

CLAUDE.md §12: keyboard-first, immediate feedback, drag and drop where natural.

## How four editors do it

| Editor | Transforms | Snapping | Selection | What works | What annoys |
|---|---|---|---|---|---|
| Blender | G, R, S start a transform at once; X, Y, Z constrain (twice: local axes; Shift+axis: the other two); typed values and expressions; an "Adjust Last Operation" panel afterwards | Ctrl toggles snapping while moving, Shift is fine; snap to increment, vertex, edge, face or volume, optionally aligning to the target's normal; pivots: median, active, individual origins, cursor | click; drag a box; C paints; Alt+click lists the objects under the cursor | numbers from the keyboard; the last operation stays adjustable | the modal keys are invisible to newcomers; many options |
| Unity | W, E, R, T, Y pick a tool; Z toggles pivot and centre, X global and local | Ctrl snaps by increments; V snaps a vertex to a vertex; Ctrl+Shift drags along surfaces | click; clicking again at the same spot cycles through overlapping objects; F frames, Shift+F follows | vertex and surface snapping; cycling picks | no typed values in the viewport; a tool must be picked before moving |
| Unreal | W, E, R, Space cycles | grid, rotation and scale steps shown as toggles with values in the viewport's tool bar; surface snapping; End drops to the floor | click, Ctrl+click toggles; Alt+drag duplicates; bookmarks on Ctrl+0..9 | snapping values always visible; End; Alt+drag | box selection behind a modifier chord |
| Godot 4 | Q select, W move, E rotate, R scale; a numeric transform dialog | Ctrl snaps while dragging; "Snap Object to Floor" | click; Alt+right click lists the nodes under the cursor | a list of what is under the cursor | numeric input needs a dialog |

## Scenarios

| The developer wants to… | Ours | Keys |
|---|---|---|
| move a crate 2 m up | G Z 2 Enter (or drag the gizmo's Y arrow) | 4 |
| rotate it 90° about Y | R Y 90 Enter (or the gizmo with Ctrl: 15° steps) | 5 |
| place a crate on a shelf | drag it from the file browser: it lands on the surface under the cursor, aligned to its normal, previewed while dragging | 1 drag |
| put ten crates in a row | Ctrl+D, G X 1.2 Enter, then Shift+R nine times ("repeat last") | ~16 |
| put a fallen prop back on the floor | End (dropped onto the surface below) | 1 |
| pick a lamp behind a pillar | click again at the same spot (cycles), or Alt+click for the list | 2 |
| change the last move from 2 to 2.5 m | the "last operation" panel by the cursor: type 2.5 | ~3 |

## What we take

1. **Selection:**
   - a click picks the nearest object under the cursor, from an id buffer the editor's
     views write;
   - Ctrl+click toggles, Shift+click adds;
   - a drag on empty space draws a box: no modifier (Godot, Figma);
   - clicking again at the same spot cycles (Unity); Alt+click lists what is under the
     cursor (Blender, Godot);
   - the selection's outline is drawn in screen space from the id buffer.
2. **Transforms:**
   - **keyboard first:** G, R, S with axis keys and typed values or expressions, in every
     key map (Blender);
   - gizmos for move, rotate, scale and all three at once (Unity's transform tool);
   - W, E, R pick the gizmo in the Godot key map;
   - Space toggles local and global axes;
   - pivots: median, each object's own origin, the active object.
3. **Snapping:**
   - steps for grid, rotation and scale shown as toggles with their values in the
     viewport's tool bar (Unreal);
   - Ctrl inverts the toggle while dragging, Shift is fine;
   - surface snapping by Ctrl+Shift drag, optionally aligned to the surface (Unity,
     Blender);
   - V snaps a vertex to a vertex (Unity);
   - End drops the selection onto the surface below (Unreal).
4. **Placement:**
   - an asset dragged in lands on the surface under the cursor, aligned to its normal,
     previewed while dragging;
   - Alt+drag duplicates (Unreal), Ctrl+D duplicates in place;
   - Shift+R repeats the last command with its values (Blender's "Repeat Last").
5. **The last operation stays adjustable** (Blender): after a command, its values sit in
   a small panel by the cursor until the next one; changing them redoes it.
6. **Frame and follow:** F frames the selection, Shift+F follows it. Bookmarks are on
   Ctrl+1..9 (`editor-layout.md`).
7. **Overlays** through debug draw channels (CLAUDE.md §14), toggled from the viewport's
   menu:
   - lights' ranges, cameras' frusta;
   - sound sources' near and far spheres, reverb zones;
   - colliders, the navigation mesh, physics contacts.

   The physics and navigation visualizers start here.
8. **Hints on screen:** while a keyboard transform runs, the axis, the typed value and the
   snapping state show by the cursor.
9. **2D:** the same tools in a 2D view later, with sprite-aware snapping.

## What we do not take

- A tool that must be picked before every transform as the only way (Unity).
- Keyboard transforms with no hint on screen (Blender's header is easy to miss).
- A dialog for typed values (Godot).
