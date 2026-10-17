# Scene tree

Checked: 2026-10-17. Products were studied as products (behaviour, documentation); Godot
(MIT) was read for ideas only. No code was carried over. Input for
[ADR 0107](../../adr/0107-editor.md).

CLAUDE.md §12: keyboard-first, everything searchable, multi-selection, drag and drop
where it feels natural, errors next to their cause, stable at any scene size.

## How five editors do it

| Editor | Making and arranging | Finding | Visibility and locks | What works | What annoys |
|---|---|---|---|---|---|
| Godot 4 (Scene dock) | Ctrl+A adds a child through a dialog with search and favourites; Ctrl+Shift+A instances a scene; drag to reparent (keeps the global transform); Ctrl+D duplicates; F2 renames | a filter box above the tree | an eye per node; icons for scripts, signals and groups on the row | everything about a node is on its row; reparenting keeps where things are | adding a node is a modal dialog; an instanced scene hides its children unless "Editable Children" is on |
| Unity (Hierarchy) | right-click to create; drag to reparent; Ctrl+D; F2; several scenes as headers in one tree | search, with type filters | eye (visibility) and hand (pickability) toggles that affect only the editor | editor-only visibility and pickability; scene headers | empty objects used as folders stay in the game |
| Unreal (Outliner) | drag to attach; folders that exist only in the editor; columns (type, level) | search and type filters | visibility per actor | editor-only folders; columns; filters | an actor's label differs from its name |
| Blender (Outliner) | drag to parent; collections as real groups; Shift+click on an arrow expands a whole branch | search and a filter popover | many restriction toggles (exclude, select, viewport, render…) | recursive expand; restriction toggles | too many display modes and toggles; collections vs parenting confuses |
| Figma (Layers) | groups and frames; drag to reorder; double-click to rename | find by name | eye and lock appear on hover only | hover-only toggles keep rows clean; Enter selects children and Shift+Enter the parent, from the canvas | deep nesting is hard to walk in big files |

## Scenarios

| The developer wants to… | Godot today | Ours |
|---|---|---|
| add an empty child under the selection and name it | Ctrl+A, type, Enter (dialog), F2, type, Enter | Ctrl+A opens an inline search on the row, type, Enter: the entity is made and its name is being edited; type, Enter |
| find "Door_12" among 5,000 entities | type in the filter, scan the shown branches | type "door 12": matches shown with their ancestors, the count beside the box, Enter selects and frames |
| put five selected objects under a new parent | right-click, "Reparent to New Node", pick a type, rename | Ctrl+G: grouped under a new entity at their median, world transforms kept, the group's name being edited |
| hide a branch while working on another | the eye on the row | hover, the eye; or H over the selection, Alt+H shows everything again |
| reparent and keep the local offset instead of the world position | not offered by drag | drag with Alt |

## What we take

1. **Rows:**
   - an icon from the entity's main component and its name;
   - badges: prefab instance, script, error or warning count. A click on a badge goes to
     the cause (§12);
   - the eye and the lock show on hover and on hidden or locked rows only (Figma). They
     are editor-only state, saved with the editor's session, never in the game's data
     (Unity).
2. **Names:**
   - a `Name` component (a string; [ADR 0108](../../adr/0108-scene-format-2.md));
   - a new entity is named after its main component ("Point light"), with a number when a
     sibling already has that name ("Point light 2").
3. **Filter, always visible:**
   - the palette's matcher (`command-palette.md`): names, then components (`:Light`) and
     prefab sources;
   - matches are shown with their ancestors, the rest hidden, with a count.
4. **Selection and keys:**
   - click, Ctrl+click toggles, Shift+click selects a range;
   - arrows move, Shift+arrows extend;
   - Left and Right collapse and expand; Shift+Right expands a whole branch (Blender);
   - F2 renames;
   - Ctrl+D duplicates, Ctrl+G groups, Delete deletes. Everything is undoable, nothing
     asks for confirmation.
5. **Drag and drop:**
   - drop on a row to reparent (the world transform kept; Alt keeps the local one);
   - drop between rows to reorder;
   - Ctrl+drag copies;
   - an asset dropped from the file browser becomes a child of the row.
6. **Viewport sync:** a pick in the viewport reveals and scrolls to its row without
   collapsing others.
7. **Groups are entities.** No editor-only folders for now: an empty entity costs one
   transform in propagation, and it means the same thing in the editor and the game.
8. **Instances:** a prefab instance's children are shown and editable directly. An edit
   becomes an override (`prefabs.md`).
9. **During Play** the tree shows the running copy of the world:
   - entities the game spawned (no scene id) are dimmed;
   - changed entities carry a dot, and their rows offer "Keep changes" (`play-mode.md`).
10. **Scale:** only visible rows are built (ADR 0030), so 100,000 entities scroll like
    100.

## What we do not take

- A modal "add node" dialog (Godot): the search opens inline.
- Hidden instance children behind a toggle (Godot's "Editable Children").
- Many display modes and restriction toggles (Blender).
- A label separate from the name (Unreal).
