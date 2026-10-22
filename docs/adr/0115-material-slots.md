# 0115. Material slots: a model's mesh of several materials

- Status: Accepted (2026-10-22). The slots on the model are the team's decision of
  2026-10-22; the way they are drawn is decided here.
- Date: 2026-10-22
- Builds on: [ADR 0108](0108-scene-format-2.md) (`Model`, scene files, E4-3's drag and
  drop), [ADR 0064](0064-gpu-scene-extraction-runtime.md) (the runtime's extraction
  into the persistent GPU scene), [ADR 0107](0107-editor.md) (the editor's undo, tree and
  picks)

## Context

- A glTF mesh's primitives often have materials of their own: a lamp's metal and glass,
  Sponza's one mesh of 25 materials.
- The player's `Model` names one mesh and one material. A glTF mesh's primitives were
  drawn as one mesh with that material (`gltf_mesh`), so such a mesh drew with its first
  material (found at E4-3).
- **The team's decision (2026-10-22):** material slots on the model, one entity per mesh, as
  Unity's renderers and Godot's surfaces have them. Not a part entity per primitive in the
  scene file and the tree.
- **The renderer** draws one mesh with one material per instance.
- **The runtime's extraction** (ADR 0064) mirrors one `MeshRenderer` per entity into one
  instance. That path is hot and tuned: runs of rows compared on the job pool, and the
  moving instances interpolated between ticks.

## Options

1. **Sections in the renderer** (Unity's submeshes, Unreal's sections, Godot's surfaces): a
   renderer of N mesh and material pairs, which the extraction makes N instances of one
   entity.
   - The editor sees one entity, as now.
   - **Cost: the extraction changes.** Its per-entity entry, its moves and its interpolation
     would carry a list of instances: a branch per entity on the path every scene runs. Its
     interpolation would have to move a part list as one, or the sections of one mesh
     would drift apart while it moves.
2. **Parts as entities the engine makes** (chosen):
   - **What draws:** the first primitive draws on the model's entity, as now. Each further
     primitive draws on a child entity that the player makes, marked
     `engine_runtime::Derived`, with a `MeshRenderer` of its own.
   - **The renderer, the extraction and the interpolation are untouched.** The parts move
     with the model through the transform propagation, so they cannot drift apart.
   - **No scene id:** scenes never save a part, and the editor's tree already lists only
     entities with scene ids.
   - **What the editor needs:**
     - undo leaves the parts out, since it would give them scene ids and record them;
     - picks and box selection name a part's model.
   - **Cost:** an entity per further primitive (a mesh of three materials: two). The
     instances drawn are the ones sections would make.
3. **Parts as entities of the scene** (the option the team turned down): a primitive per
   entity in the file and the tree, each with its own `Model`. The tree grows with every
   imported mesh, and a mesh renamed or re-split in its file breaks the scene.

## Decision

- **`MaterialSlots(Vec<AssetRef>)`**, a component beside `Model` on the same entity, holds
  the materials of the mesh's primitives after the first, in the mesh's order.
  - The first primitive's material stays `Model::material`.
  - A primitive past the list draws with `Model::material` too.
  - **Without slots** (the default, and every scene written before) the primitives draw as
    one with `Model::material`, as before.
  - A component of its own rather than a field: `Model` stays plain data. The player's
    change query on models, the inspector and every scene so far are unchanged, and a model
    of one material pays nothing.
  - Scenes write it as a list of asset references:

    ```text
    MaterialSlots = [asset("props/lamp.glb#Glass", 5f2c0a3e9d2b47c88e51f0d4a7b3c912)]
    ```
- **The player** (`Assets`):
  - a glTF mesh of a model with slots is made a mesh per primitive: in the background, as
    meshes are (`Job::Primitives`), and kept per asset;
  - the entity's `MeshRenderer` draws the first primitive with `Model::material`;
  - each further primitive gets a child, which is `Derived` and marked with its primitive's
    index. It has an identity transform and its own `MeshRenderer`. Children are made,
    changed and despawned as the model, its slots and its mesh change, and are hidden with
    the model;
  - slots taken away draw the mesh whole again;
  - parts whose model went or draws whole again are despawned by a sweep in the same
    frame. A model despawned without its children leaves them a dead parent, and the sweep
    catches that too;
  - a file read again (E4-5) keeps its old per-primitive meshes drawn until the new ones
    are made, as for the other parts;
  - built-in meshes have one primitive: their slots do nothing.
- **The editor:**
  - **Drag and drop:** a model dropped from the dock takes its glTF's further primitives'
    materials into its slots, read from the file's outline. A mesh of one material gets
    none. The status line's note that such a mesh draws with its first material goes.
  - **Undo** leaves derived entities out of the subtrees it records, deletes and copies.
    That covers a part referring to a deleted model as its parent. The player makes parts
    again for whatever undo puts back.
  - **Picks and box selection** name a derived entity's nearest ancestor with a scene id,
    once.

## Consequences

- A mesh of several materials draws as its file has it once dropped into the editor, or
  once a scene gives its model the slots.
- Models of one material cost nothing more: the queries for slots and parts visit no
  entities when no model has slots.
- Left:
  - editing slots in the inspector (it has no list fields yet);
  - a material dropped on one primitive's surface going to that slot;
  - previews in the dock drawing every slot;
  - the cooked runtime's scenes (the one-button build, ADR 0107 E14).
- If parts ever cost too much (a mesh of hundreds of materials), sections in the renderer
  (option 1) can replace them without changing scene files.
