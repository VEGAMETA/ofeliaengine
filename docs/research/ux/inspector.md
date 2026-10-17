# Inspector

Checked: 2026-10-17. Products were studied as products (behaviour, documentation); Godot
(MIT) was read for ideas only. No code was carried over. Input for
[ADR 0107](../../adr/0107-editor.md).

CLAUDE.md §12: multi-selection and multi-editing, immediate feedback, everything undoable,
errors next to their cause, keyboard-first.

## How six editors do it

| Editor | What works | What annoys |
|---|---|---|
| Unity (Inspector) | components as foldouts with a menu (reset, copy and paste values); multi-editing shows "—" for values that differ; prefab overrides in bold with a blue bar in the margin; "Add Component" search with categories; a lock to keep showing one object; a floating properties window for one object | good editing of a type often needs a hand-written custom editor |
| Unreal (Details) | a search box; "show only modified properties"; "Multiple Values" for multi-selection; a reset arrow per changed property; a property matrix for bulk edits in a grid; favourites pinned to the top; a lock | very long panels with large categories |
| Godot 4 (Inspector) | a filter box; a revert icon on every changed property; sub-resources edited inline; multi-node editing of shared properties; back and forward through inspected objects; a key icon to animate a property | a shared resource edited in one place changes it everywhere until "Make Unique"; deep sub-resource nesting |
| Blender (Properties) | drag a number to scrub (Ctrl snaps, Shift is fine); expressions in fields; Backspace over a field resets it; Ctrl+C and Ctrl+V over a field copy a value; "Copy to Selected"; I over a field keys it; field colours show keyed or driven values | editing all selected objects needs Alt (Alt+drag, Alt+Enter), which few discover |
| Figma (properties) | "Mixed" for differing values; scrubbing by dragging a field's label; maths in fields; "Selection colors" lists the colours a selection uses and edits each everywhere at once | none of note for us |
| Houdini (parameters) | a value ladder (middle button on a field: drag at a chosen magnitude); expressions and references in any field; Ctrl+middle click resets | dense for newcomers |

## Scenarios

| The developer wants to… | Ours |
|---|---|
| give 20 selected lights the same colour | the inspector shows their shared components, differing values as "—"; set the colour once: all 20 change, one undo step |
| find "shadow bias" among 60 fields | type "bias" in the filter: the matching fields across components stay |
| make five platforms 20 % wider | select them, type `*1.2` into scale X: each multiplies its own value |
| try values while playing and keep the best | edit during Play; the changed field is marked; its "keep" button copies it into the scene (`play-mode.md`) |
| put a component back to its defaults | the component's menu, Reset; or ↺ on one field; or Backspace over the field |
| copy a collider's settings to ten other props | the component's menu, Copy; select the ten, Paste values |
| add a point light to an entity | "Add component" (or Ctrl+Shift+A), type "point", Enter |

## What we take

1. **Generated from reflection** (ADR 0022) by kind:
   - numbers and booleans;
   - enums as a dropdown, or segmented buttons when there are few;
   - vectors as one row with coloured axis labels;
   - colours (`hint = color`), angles shown in degrees (`hint = degrees`);
   - strings (`multiline` gives a text area);
   - lists (add, remove, reorder rows), maps, optional values (a check box and the
     value);
   - entity references: a pick button, or drag a row from the tree;
   - asset references: drop from the file browser, or search inline.
2. **New reflection attributes:**
   - `unit` (m, s, °, dB, Hz) for display and typing;
   - `group` for sub-headings inside a long component.
3. **The filter is always visible.**
   - It matches field names, component names and tooltips.
   - "Modified only" shows values that differ from the default or from the prefab
     (Unreal).
4. **Multi-editing needs no modifier.**
   - The selection's shared components are shown, and differing values read "—" (Unity,
     Figma).
   - Typing a value sets it on every entity.
   - Scrubbing and relative expressions (`+=0.5`, `*1.2`) change each entity's own value.
5. **Number fields:**
   - scrub by dragging the value. Modifiers are the owner's (2026-10-17), not Blender's:
     free by default, Shift takes the other mode for the moment (whole steps from the
     start), Ctrl rounds to the grid; steps position 0.1, scale 0.1, rotation 45°, else
     the field's `step` (built in E1-4a, ADR 0107);
   - Up and Down step;
   - expressions (`2*pi`, `+=10`, `*2`);
   - units shown and accepted (`30deg`).
6. **Reset and copy:**
   - ↺ on every changed field (Godot, Unreal);
   - Backspace over a hovered field that is not being typed in (Blender);
   - Ctrl+C and Ctrl+V over a hovered field (Blender);
   - Copy and Paste values per component (Unity);
   - "Copy to selected" per field (Blender).
7. **Components** appear in a fixed order: `Transform` first, then by category. Order
   means nothing in an ECS, so there is no manual reordering. Each header carries a
   menu: reset, copy, paste, remove.
8. **Lock and more inspectors:** lock the inspector to an entity, or open another
   inspector locked to one (Unity, Unreal).
9. **Errors next to their cause:** a bad value (a missing asset, a range the type
   refuses) is shown on its field in red, with the reason.
10. **Undo:** one step per edit. A drag merges into one step (ADR 0031's merge key).
11. **Live:** the inspector reads the world every frame (ADR 0030), so during Play it
    shows live values. Edits there are marked against the scene, with "keep" per field
    (`play-mode.md`).
12. **Prefab overrides:** bold label and a bar in the margin (Unity), ↺ back to the
    prefab, "apply to prefab" (`prefabs.md`).
13. **Animation:** keying from a field (Blender's I, Godot's key icon) comes with the
    animation editor.
14. **Speed:** "—" is worked out only for visible fields, so a selection of 1,000
    entities costs what its visible rows cost.

## What we do not take

- Hand-written per-type editors as the main path (Unity's custom editors). Attributes
  cover most needs; custom widgets come later as an extension point.
- Multi-editing behind a modifier (Blender's Alt).
- Shared sub-objects that change everywhere when edited in one place without a warning
  (Godot's "Make Unique").
