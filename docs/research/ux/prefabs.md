# Prefabs

Checked: 2026-10-17. Products were studied as products (behaviour, documentation); Godot
(MIT) was read for ideas only. No code was carried over. Input for
[ADR 0108](../../adr/0108-scene-format-2.md) and [ADR 0107](../../adr/0107-editor.md).

The owner, 2026-10-17: prefabs are in the editor's first version.

## How four editors do it

| Editor | What a prefab is | Overrides | Editing the source | What works | What annoys |
|---|---|---|---|---|---|
| Unity | a prefab asset; nested prefabs; variants (a prefab based on another with overrides) | bold labels and a blue bar in the margin; an "Overrides" list on the instance to apply or revert each, or all; added and removed components and children are overrides too | "prefab mode": in isolation or in the scene's context | overrides are visible; apply and revert per item; variants | which level an override is applied to in nested prefabs is easy to get wrong; asset GUIDs in `.meta` files |
| Godot 4 | any scene can be instanced in another; inherited scenes | a revert icon per changed property; children editable only with "Editable Children"; "Make Local" | the scene opens in its own tab | one kind of file for levels and parts; familiar to the team | overrides follow node paths, so renaming a node in the source drops them; hidden children by default; inherited scenes confuse |
| Unreal | Blueprint classes (components, defaults and logic); Level Instances and Packed Level Actors for level pieces | a reset arrow on changed properties | the Blueprint editor | defaults propagate unless overridden | heavy for "a group of objects": compile steps, a separate editor |
| Figma | components and instances; variants (component sets with properties) | kept when the main component changes; reset per property or all; detach; instance swap | "Go to main component" | overrides are obvious and safe | overrides match layers by name and structure, so renames can drop them |

## Scenarios

| The developer wants to… | Ours |
|---|---|
| make a prefab of a selected subtree | "Make prefab" (palette, or drag the subtree into the file browser): a scene file is written and the subtree becomes an instance of it, one undo step |
| place 20 crates, recolour one, then change the crate's mesh | all 20 take the new mesh; the recoloured one keeps its colour |
| see what an instance changes | bold labels and margin bars in the inspector; the instance's header lists every override, each with revert and apply |
| edit the prefab itself | double-click its badge: it opens in a tab; every open scene follows its changes live |
| make a red door from the door | "Make variant": a new scene whose root is an instance of the door; recolour it |
| cut the link | "Unpack": the instance becomes plain entities, one undo step |

## What we take

1. **Any scene can be instanced** (Godot): one kind of file for a level, a room and a
   crate, and nesting falls out of it.
2. **Overrides follow stable ids, not names** (against Godot's and Figma's pitfalls).
   Every entity in a scene already has a 128-bit id (ADR 0029); an override names the
   instance and the entity's id inside the prefab.
3. **The inspector shows where a value comes from** (Unity):
   - bold label and a margin bar on overridden fields;
   - ↺ goes back to the prefab's value;
   - "apply to prefab" writes the value into the source.
4. **An instance's children are shown and editable** without a toggle. An edit becomes an
   override.
5. **Structure changes are overrides too** (Unity): children and components added to an
   instance, and the prefab's entities or components it removes.
6. **Nesting and variants:** a prefab may contain instances; a variant is a scene whose
   root is an instance with overrides.
7. **Applying in nested prefabs** goes to the nearest prefab by default. The menu names
   every level, so the target is never a guess (Unity's ambiguity).
8. **Editing the source** in its own tab (Godot). Open scenes follow at once. Editing in
   the scene's context can come later (Unity).
9. **Unpack** to plain entities (Unity's Unpack, Godot's Make Local, Figma's Detach).

## What we do not take

- Overrides matched by names or paths (Godot, Figma).
- Children hidden behind a toggle (Godot's "Editable Children").
- A separate class editor for simple groups of objects (Unreal's Blueprints).
