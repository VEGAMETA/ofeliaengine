# Materials

Checked: 2026-10-22. Products were studied as products (behaviour, documentation); Godot
(MIT) was read for ideas only: its documentation and, at 4.4-stable, its material
preview's source. No code was carried over. Input for [ADR 0107](../../adr/0107-editor.md)
§7, phase E10, after [ADR 0092](../../adr/0092-custom-materials.md) (custom materials) and
[ADR 0112](../../adr/0112-asset-previews.md) (previews).

CLAUDE.md §9: a material graph that generates Slang, hand-written Slang with full support,
few permutations, hot reload with the previous version kept on an error. §12: everything
undoable, immediate feedback, errors next to their cause. ADR 0107's E10: "graph and code
(Slang) with a live preview, instances, previews in the browser".

## Where the engine starts

- **A material is a Slang type** implementing `IMaterial` (ADR 0092, done): shading models
  lit, unlit, clear coat, cloth, hair, toon and glTF's sheen; parameters laid out from
  Slang's reflection; the glTF fallback drawn while a type's pipelines compile; a module
  edited to on screen in 240–266 ms, a material file in 31 ms (no compile); a compile
  error keeps the previous version and is kept at its line; PSO records; packs.
- **Two material files.** `.material` (ADR 0108, the editor's): glTF's colours and numbers,
  no textures, no type. `.material.ron` (ADR 0092, confirmed by the team on 2026-10-12): a
  type, its parameters, textures, a fallback; read by the cook, `engine_streaming` and the
  golden tests. The editor's player (`engine_app::Assets`) reads only `.material`, so typed
  materials do not draw in the editor yet.
- **The editor** (E4, ADR 0112, ADR 0115): thumbnails and inspector asset fields drawn by
  the engine (a material on the built-in sphere, 128², one a frame, cached); drag and drop
  onto objects and fields; `#material:`; Find References; material slots, not editable in
  the inspector yet. A material file picked in the dock shows only its path and kind.
- **E8's code editor** knows Luau only. **Driver statistics** (registers, spills, through
  `VK_KHR_pipeline_executable_properties`) feed the perf gate (ADR 0062); the profiler
  panel shows GPU passes, not materials (E6-5).

## How five tools do it

| Tool | What it has | What works | What annoys |
|---|---|---|---|
| Unreal Engine 5.8 (Material Editor, material instances) | a graph per material; Material Functions; a Custom node for HLSL; a read-only HLSL view; instances whose scalar, vector, texture and static switch parameters each have an override check box; a preview on a cylinder, sphere, plane, cube or any mesh; node thumbnails and "Start Previewing Node"; Stats (instructions of the costliest vertex and pixel shader, samplers, texture samples, interpolators), Platform Stats, the Shader Complexity view; Substrate's layered BSDFs (production-ready since 5.7) | instance edits "appear in the main viewport immediately", with no compile; an edit of a function reaches every material that uses it; the parent chain one click away (Hierarchy); the cost beside the graph | graph edits reach the level only through Apply, and a material "could take up to a minute to recompile"; an unchecked parameter shows a value that is not used and is dropped on close; a shader for "every combination of static parameters"; the Custom node "prevents constant folding"; Shader Complexity counts instructions only ("may not always be accurate"); All Node Previews "can be time-consuming" |
| Unity 6 (Shader Graph 17, material variants) | the Blackboard (properties, keywords), the Graph Inspector, a Master Stack (vertex, fragment), a Main Preview (primitives or a custom mesh) and previews on most nodes; Sub Graphs ("Convert To Sub-graph"); a Custom Function node (a string or an `.hlsl` file, its ports declared by hand); "Copy Shader"; the Heatmap colour mode; variants: a parent, a line beside each override, "Revert", "Apply as Override in…", "Lock in children" | node previews show where a value goes wrong; Space opens the node search, and a link dropped in empty space opens it filtered to compatible ports; variants work like prefab variants | scene materials change on "Save Asset"; previews "can't access Unity's Render Pipeline libraries", so they can differ from the scene; keywords can mean "millions or trillions of shader variants"; the Heatmap is "broad estimates", texture samples of unknown cost |
| Godot 4.7 (visual and text shaders) | visual shaders turned into a text shader "behind the scene" ("show generated code"); a text editor that "will detect errors as you type"; an Expression node; reroute and frame nodes (4.3); a preview of one output port per node; uniforms shown as inspector parameters with hints (`source_color`, `hint_range`, `hint_enum`, `group_uniforms`, doc comments as tooltips); per-instance uniforms (at most 16, no textures or arrays); "Convert to ShaderMaterial" (one way, to a text shader); a material preview in the inspector (sphere, box, quad; two lights) | parameters described in the shader itself; edits live in the scene; the team's layout keeps the shader editor in the bottom bar, under the scene (`editor-layout.md`) | no sub-graphs in stable: groups (no parameters, no varyings) exist only in the development docs; a material shared by many objects is edited for all of them from any one ("Make Unique", `inspector.md`) |
| Blender 5.2 (shader nodes) | node groups (Ctrl+G, Tab, Ctrl+Alt+G; sockets in panels; nested; linked or appended from other files); frames, reroutes; link-drag search (3.1); Node Wrangler, bundled but off by default: Shift+Ctrl+click shows a node's output on the object, Ctrl+T and Shift+Ctrl+T wire textures from files; the Material Preview viewport mode (EEVEE, an HDRI) | the real object is the preview, and any node shows on it in one click; groups are functions with typed inputs | the best preview tool is an add-on to turn on; groups from other files come through Link/Append, kinds not separated (the manual suggests `Mat_`, `Geo_` prefixes); no instances: copies, or a group's inputs |
| Roblox Studio (Material Manager) | variants of the built-in materials (colour, normal, metalness and roughness maps, "Studs Per Tile", "Pattern"); "Apply to Selected Parts"; "Set as Override" replaces a base material across the place, terrain included | a textured variant made and put on the selection in a few clicks, with no code | a variant is found by its name: renamed after use, it must be applied again |

## Scenarios

| The developer wants to… | Ours |
|---|---|
| make a red variant of the car paint and tune its roughness on the car | select the car; under `Model.material`, Make Variant: a copy beside the file, given to the car in one undo step; drag roughness: the car changes by the next frame, nothing compiles; Ctrl+S. 4 actions (Unreal: create an instance, name it, open it, tick the override, drag, save, assign: 8) |
| change a material 40 props share | select one: the material's parameters unfold under `Model.material`, marked "used by 40 objects in 3 scenes"; drag a value: all 40 change. 2 actions |
| write a toon water surface in Slang | "Material: New Type" and a name: `water.slang` opens from a template at `surface`, and `water.material` goes to the selected object; a typo is underlined at its line a moment after typing stops; Ctrl+S: on screen in about 0.25 s, the old version drawn until then and on an error |
| fix an error the Output reported | click the row: the module opens at its line (E6-2), or the graph selects the node that made it. 1 action |
| put moss on the tops of rocks without code | "Material: New Graph" on the selected rock: the graph opens under the scene, wired with the rock's own glTF textures; Space, "normal"; a link dropped in empty space, "smoothstep"; the moss texture dropped from the dock becomes a node; Space, "lerp"; three links. Each edit shows on the rock in about 0.25 s. About 9 actions |
| find why the water costs 2 ms on the iGPU | its Stats: per variant drawn, registers and spills from the driver, instructions, image samples; Draw as Fallback: the profiler's forward pass shows what the water costs over plain shading, on this GPU. 2 actions |
| see a material on a sphere, a plane and the real model | a click on its preview cycles the built-in meshes and the selected entity's model; the scene shows it anyway, since edits are live. 1 action |
| reuse triplanar mapping in other materials | select its nodes, Ctrl+G: a function file, found in every graph's node search; or a Slang function marked `[Node]`, which is a node as written. 1 action |

## What we take

1. **Live, with no Apply.** A parameter edit reaches the material table by the next frame
   (ADR 0040), with no compile. A code or graph edit compiles only the variants drawn and
   swaps them (ADR 0092: 240–266 ms, budget 500 ms); the previous version draws until then
   and on an error, and a new type shows its fallback meanwhile. Ctrl+S writes files; it is
   not needed to see a change (against Unreal's Apply and Unity's Save Asset).
2. **One material file** for the editor, the player and the cook (question 1): a type (a
   Slang module or a graph), its parameters by name, textures as asset references, the
   fallback, alpha, double-sided. A file without a type is glTF's PBR, as `.material` is.
3. **Parameters in the inspector,** generated from Slang's reflection of the type's
   `Params`, by kind as for components (`inspector.md`): colours, numbers, vectors,
   textures as asset fields with previews. Ranges, colour hints, groups and tooltips come
   from Slang user attributes on the fields (Godot's `hint_range`, `source_color`,
   `group_uniforms`, doc comments). The pinned Slang 2026.18.3 reflects user attributes;
   a probe test checks it before E10-1 relies on it. ↺ puts a field back to its default.
4. **Edits are undo steps** in the one history; the file is written by Ctrl+S and
   autosaved as a scene is (ADR 0107 §4). Unlike import settings (E4-9), materials are
   content, edited all day.
5. **Edited from the object** (Godot's sub-resources): a selected entity's `Model.material`
   and slots unfold their parameters, with how many objects and scenes use the material
   (E4-7's references). Make Variant copies it beside the file and gives the copy to the
   selection in one step. Nothing shared changes silently (`inspector.md`).
6. (Replaced on 2026-10-28, ADR 0123: shader code and graphs open as files in tabs of
   their own, the team's decision.) **Shader code and graphs open in the Shader panel** under the viewport (the team's
   layout), so the scene stays in sight; the panel can be maximised or moved like any dock.
   Code uses E8's core with Slang highlighting. Slang's front end checks the text after a
   pause (0.12 s for `forward.slang` with a type, ADR 0092), Ctrl+S compiles, and errors
   are underlined at their lines and listed in the Output (E6-2). "Open in VS Code" keeps
   Slang's own extension; its language server, `slangd`, ships with the pinned Slang and
   is the candidate for completion later.
7. **A graph is a material type,** as a module is: it generates one Slang module with a
   type implementing `IMaterial`, then goes through the same compile, errors, hot reload,
   PSO records and cook. Its output node is `Surface`, with one of ADR 0092's shading
   models. An error in generated code marks the node that made the line. "Show Slang"
   opens the module read-only; "Convert to Code" is one way (Godot's convert).
8. **Code inside graphs:** a function of the project's Slang modules marked `[Node]` is a
   node, its ports read from Slang's declaration reflection (in the pinned release; the
   engine's C binding would need it added), so nothing is declared twice (Unity's Custom
   Function needs its ports by hand). An expression node covers one-liners (Godot's
   Expression).
9. **Functions:** Ctrl+G makes one from the selection, Tab enters it, Ctrl+Alt+G takes it
   apart (Blender's keys). It is a file with an id; an edit recompiles the drawn materials
   that use it.
10. **A keyboard-first canvas:** Space at the cursor opens the node search (the palette's
    matcher); a link dropped in empty space opens it filtered to compatible ports (Unity,
    Blender since 3.1); textures dropped from the dock become nodes; frames, reroutes, F to
    frame. The canvas is `engine_ui`'s, reused by E12's state machines.
11. **Previews by the engine** (ADR 0112): the material's preview at the top of its
    inspector, on a built-in mesh, the selected entity's model or a model dropped on it,
    orbited by dragging (Unreal, Unity). Per-node previews draw every shown node
    into its tile of one atlas from one generated entry point, so an edit costs one
    compile, not one a node (to be measured). A material dragged over an object shows on
    it before the drop (E4-3's left item).
12. **Cost from measurements:** per variant drawn, the driver's registers and spills, the
    SPIR-V's instructions and image samples, the parameters' bytes and the compile time.
    Draw as Fallback draws the material with its glTF fallback, so the profiler's forward
    pass shows its cost over plain shading in this view on this GPU. Registers come first:
    on the iGPU they decide (CLAUDE.md §5).

## E10's steps

| Step | What | Why there |
|---|---|---|
| **E10-1. Materials in the inspector** | the one material file; typed materials made by the player (the type registered, its drawn entry points compiled off the frame, the fallback meanwhile); parameters in the inspector from the object and from the dock, live and undoable; New Material, Make Variant, Apply to Selection (Roblox's); slots as asset fields (ADR 0115's left item); thumbnails redrawn after an edit, a typed one once its preview variant is compiled (one more entry point per type, about 0.2 s), not from its fallback | most material work is tweaking materials that exist (Unreal's docs steer it to instances: no compile, changes at once); typed materials cannot draw in the editor until this lands; the inspector, previews, undo and reloads exist |
| **E10-2. Slang in the editor** | `.slang` in the Shader panel; Slang highlighting; New Material Type from a template; the front end's errors after a pause, compiles on Ctrl+S; a field added to `Params` in the inspector after the save, values kept by name | the hard parts exist (ADR 0092's reload and errors, E8's editor), so it is mostly wiring, and it gives water or toon early; E10-3 reuses its compile and error path |
| **E10-3. The graph** | the canvas; nodes, links, parameters into `Params`; module functions as nodes; functions; the generated module; errors on nodes; numbers on nodes as question 3 decides | the largest new interface; it stands on E10-1's parameters and E10-2's path, and the scene is its first preview (Unreal ships "All Node Previews" off) |
| **E10-4. Previews and cost** | the orbiting preview; per-node previews; stats; Draw as Fallback; the preview while dragging; variants with a base (question 2) | each makes the first three faster to use and none blocks them; cost matters once materials exist |

## What we do not take

- **An Apply step** (Unreal) **or a save before the scene changes** (Unity's Save Asset).
- **Static switches and keywords** that multiply shaders by combination (Unreal, Unity).
  When a game needs switches, they are generic value parameters of the type (Slang's `let`
  generics), each combination listed with its compile cost.
- **Cost from instruction counts or fixed tables** (Shader Complexity, Unity's Heatmap):
  registers and measured time instead.
- **Override check boxes** (Unreal's instance editor shows a value that is not used while
  its box is unchecked): a value that differs from its default or its base is marked, ↺.
- **Per-object parameters** (Godot's `instance uniform`: at most 16, no textures): our
  instances are 64 bytes on a tuned path (ADR 0040, ADR 0064), and a variant costs a slot
  in the material table, not a pipeline. Reopened when a game needs thousands of tints.
- **Layered materials** (Unreal's Material Layers, Substrate): a renderer decision beyond
  ADR 0092's models; a graph can blend surfaces itself.
- **Previews outside the renderer** (Unity's): ours are the engine's own (ADR 0112).
- **Graphs made from code:** conversion goes one way; code enters a graph as code nodes.

## Questions for the team

1. **One material file.** Two files describe materials today. `.material.ron` holds types,
   parameters and textures; the team confirmed it on 2026-10-12, before the editor's own
   grammar existed (ADR 0108, 2026-10-17). The editor's `.material` holds only glTF's
   colours and numbers, and the editor does not read `.material.ron`.
   - a. `.material` grows to hold everything `.material.ron` holds; the cook reads it; the
     six golden files are converted and `.material.ron` is retired. Graph and function
     files use the same grammar. Cost: a converter and the cook's map change; in return,
     one grammar for every file the editor writes, with ids inside the files (ADR 0108).
   - b. The editor reads and writes `.material.ron`, its id in a `.meta`. Cost: a second
     grammar in the inspector's save path, comments and order kept by hand, and a `.meta`
     beside our own files, against ADR 0108.
   - c. Both stay. Cost: two readers, two reload paths, developers choosing between them.
   - Recommendation: a.
2. **Variants.** Ten materials that differ only in colour: should they share the rest?
   - a. No: Make Variant copies the file. Cost: nothing now; a later change to all ten is
     made ten times.
   - b. Yes: a material file may name its `base` and holds only what it changes, marked as
     prefab overrides are (E5: bold label, bar, ↺, Apply to Base). Cost: one field,
     chains resolved at load (cycles refused, as for prefabs), flattened by the cook.
   - c. b, plus locks (Unity's "Lock in children"). Cost: more interface, no need seen.
   - Recommendation: b, in E10-4: the copy ships first, and the field is an addition.
3. **Numbers typed on graph nodes** (0.3 for a roughness, say). Compiled into the shader,
   a number costs nothing when the game runs, but every drag recompiles (about 0.25 s
   each). As a parameter, it drags freely, but costs a load, and maybe registers, on every
   pixel of the shipped game.
   - a. While a graph is open its numbers are parameters; saving compiles them in. Cost:
     one compile at save, and the stats while editing are the editing version's.
   - b. Always parameters. Cost: paid in the shipped game, against the register rule
     (CLAUDE.md §5).
   - c. Always compiled in (Unreal). Cost: 0.25 s behind every drag.
   - Recommendation: a, after measuring a toon and a water graph's registers both ways.
4. **The generated Slang.** Where does a graph's code live?
   - a. Only in the local cache and in packs; Show Slang opens it read-only. Cost: a
     graph's review is its graph file's diff.
   - b. Written beside the graph and committed. Cost: two files to keep in step, merges of
     generated code, hand edits lost on the next generation.
   - Recommendation: a.

## Sources (checked 2026-10-22)

- Unreal Engine 5.8, https://dev.epicgames.com/documentation/en-us/unreal-engine/ +
  `unreal-engine-material-editor-ui`, `-user-guide`,
  `unreal-engine-material-instance-editor-ui`, `instanced-materials-in-unreal-engine`,
  `previewing-and-applying-your-materials-in-unreal-engine`,
  `material-functions-in-unreal-engine`, `custom-material-expressions-in-unreal-engine`,
  `viewport-modes-in-unreal-engine`, `python-api/class/MaterialStatistics`; Substrate in
  5.7: https://www.cgchannel.com/?p=170937 (2025-11-12).
- Unity, https://docs.unity3d.com/Packages/com.unity.shadergraph@17.0/manual/ +
  `Shader-Graph-Window`, `Color-Modes`, `Keywords-concepts`, `Sub-graph`, `Node`,
  `Custom-Function-Node`, `Main-Preview`, `Create-Node-Menu` (`Create-Sub-Graph` in 17.3);
  Unity 6.6, https://docs.unity3d.com/Manual/ + `materialvariant-concept`, `-tasks`,
  `-properties`.
- Godot 4.7, https://docs.godotengine.org/en/stable/ + `tutorials/shaders/visual_shaders`,
  `tutorials/shaders/shader_reference/shading_language`, `classes/class_visualshadernode`;
  `en/latest/classes/class_visualshadergroup`; `en/4.3/tutorials/shaders/shader_materials`;
  `editor/plugins/material_editor_plugin.cpp` at 4.4-stable, read for ideas only.
- Blender, https://docs.blender.org/manual/en/ + `latest/interface/controls/nodes/groups`,
  `latest/editors/3dview/display/shading` (5.2), `4.2/addons/node/node_wrangler`;
  link-drag search: commit 11be151d58 (2021-12-15, Blender 3.1).
- Roblox, https://create.roblox.com/docs/parts/materials.
- Slang 2026.18.3, the pinned release (Apache-2.0 with LLVM exception): `include/slang.h`
  (user attributes on fields and functions, `IModule::getModuleReflection`,
  `DeclReflection`), `bin/slangd.exe`.
