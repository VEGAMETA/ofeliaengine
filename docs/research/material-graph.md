# Material graphs (E10-3)

Checked: 2026-10-28. Products were studied as products (their documentation); open-source
code was read for ideas only: Godot 4.7's visual shaders (MIT), three.js's node materials
and TSL (MIT), Material Maker 1.6 (MIT), egui-snarl 0.12 (MIT). Blender (GPL) was studied as
a product only. No code was carried over. Input for ADR 0123, after
[the materials UX note](ux/materials.md) (E10's scenarios and the team's answers of
2026-10-22) and [ADR 0092](../adr/0092-custom-materials.md) (material types).

## Where the engine starts

- **A material type is a Slang module** with a type implementing `IMaterial` (ADR 0092): its
  `Params` laid out by Slang's reflection, its entry points compiled off the frame, the
  previous version drawing until a new one is made and on an error, the module's save on
  screen in about 0.38 s on both GPUs (the frame's hot reload test, 2026-10-28).
- **A material file** (`.material`, ADR 0120) names the module and the type and holds the
  parameters by name; the inspector edits them live, one undo step each (E10-1c).
- **E10-2:** Slang in code tabs (front-end checks, `slangd`), Material: New Type.
- **The team's answers** (2026-10-22, the UX note's questions):
  - one grammar for every file the editor writes, graph and function files included (1a);
  - variants with a base later, in E10-4 (2b);
  - numbers on graph nodes are parameters while the graph is open and compiled in at save,
    after measuring registers both ways (3a, measured below);
  - the generated Slang lives only in the local cache and in packs; Show Slang opens it
    read-only (4a).

## How others do it

| Tool | Data and code generation | Types between ports | What we take | What we leave |
|---|---|---|---|---|
| Godot 4.7 visual shaders | a `VisualShader` resource: nodes by integer id, each with a position; connections as (from node, from port, to node, to port). Each node writes its own code given the variables of its inputs and outputs; an unconnected input uses the port's default value. The whole graph is "converted to a script shader behind the scene" ("show generated code") | scalar (float, int, uint), 2/3/4-vectors, boolean, transform, sampler, each with its colour; a scalar into a vector fills every component; a vector into a scalar takes "the average of the vector's components"; booleans become 0 or 1 | ports by kind with colours; defaults on unconnected inputs; generated code visible; an Expression node of shader code with declared ports; reroutes | integer node ids (merges of two people's edits collide); a vector averaged into a scalar (it costs, and it surprises next to Slang's own truncation) |
| Unity 6 Shader Graph 17 | `.shadergraph` (JSON); a Master Stack (vertex, fragment) as the output; Blackboard properties become the material's; Sub Graphs; a Custom Function node with its ports declared by hand | "Dynamic Vector" ports truncate every connected edge to the smallest width, a scalar promoted instead; other ports truncate (excess channels dropped) or promote (missing channels default) | truncation and promotion as Slang does them; the output as one node listing the surface's terms; properties as the material's parameters | JSON with generated ids per object (hard to read in a diff); ports declared twice for custom code |
| Unreal Engine 5.8 Material Editor | material expressions in the asset, translated to HLSL per material and its static switches; named parameters (scalar, vector, texture, static switch) become instance overrides; constants are folded, a Custom node "prevents constant folding"; Material Functions | float1-4 with "appending" and component masks; a single float broadcasts, other differing widths are errors in arithmetic | numbers folded into the code where they are not parameters; functions as assets an edit of which reaches every user | static switches multiplying shaders; Apply before the scene sees an edit |
| three.js node materials (TSL), r180 | nodes as objects; the `NodeBuilder` builds in three stages: setup (a node may expand into others), analyze (the node hierarchy walked "for optimizations in the code generation stage"), generate (WGSL or GLSL) | the node types' own conversions (`.toVec3()`, swizzles) | three passes: expand, analyze (for us: count each node's uses), write | building at run time in JavaScript |
| Material Maker 1.6 | most nodes are GLSL snippets; connecting them generates one combined shader rather than an image per node; user nodes by grouping nodes or by writing GLSL | GLSL's | snippets of the target language as the node library's body | its texture-baking purpose (ours shade at run time) |
| Blender 5.2 shader nodes (GPL: the product only) | node trees in the `.blend`; node groups as functions (Ctrl+G, Tab, Ctrl+Alt+G) with interface sockets; frames, reroutes; link-drag search since 3.1 | implicit conversion between socket types | the group keys; frames and reroutes | the binary file |
| egui-snarl 0.12 (MIT, a node canvas for egui) | the graph's data apart from its view; pins inside, on or outside a node's frame; wires as curves whose formula can be styled; Ctrl+drag from a pin yanks its wires to another pin; serde | the application's | the data apart from the view; yanking wires off a pin | egui itself: the editor's UI is our own (ADR 0030) |

## Measured on ref-1: numbers compiled in or read as parameters (question 3)

What a toon graph and a water graph would generate, each twice
(`bench/graph-numbers/graph_numbers.slang`): the toon an albedo texture tinted, three bands,
a highlight and a rim; the water two normal-map samples at world scales blended, a colour by
height, glossy and a little transparent. `*Literals` has its numbers in the code;
`*Numbers` reads them from its parameters, as while a graph is open. Textures are
parameters in both. `ENGINE_PIPELINE_STATS=1 cargo run --release -p engine_render --example
material_registers -- --module bench/graph-numbers/graph_numbers.slang --types
ToonLiterals,ToonNumbers,WaterLiterals,WaterNumbers` (2026-10-28, `main` at 13e2655):

| Fragment shader registers, numbers compiled in → as parameters | plain | local lights | environment | lights + environment |
|---|---:|---:|---:|---:|
| RTX 4060 Ti (NVIDIA 617.42, Register Count): toon | 40 → 48 | 48 → 56 | 44 → 56 | 48 → 56 |
| RTX: water | 48 → 55 | 48 → 56 | 54 → 56 | 54 → 56 |
| Radeon iGPU (26.8.1, VGPRs): toon | 40 → 45 | 50 → 60 | 55 → 55 | 63 → 72 |
| Radeon: water | 35 → 43 | 54 → 54 | 54 → 54 | 73 → 74 |
| (the engine's glTF shading: RTX; Radeon) | 56; 47 | 63; 64 | 63; 64 | 56; 80 |

- **Parameters cost registers** wherever the compiler could fold a literal: up to +12 on the
  RTX, +10 on the Radeon. A loaded number cannot be folded into the toon's bands or the
  water's scales.
- **The Radeon's toon with lights and the environment goes from 63 to 72 VGPRs,** across
  the 64 that cost the forward pass 9–13 % when dead code under a specialization constant
  did the same (CLAUDE.md §5).
- **So 3a holds:** unnamed numbers are literals in the saved build; while a graph is open
  they are parameters of an editing build, which costs that much more until it is saved.
  A number the developer names is a material's parameter in both, at that cost.

## What we take

1. **A graph is a material type,** as a module is: the player generates one Slang module
   from it (a type implementing `IMaterial`) and compiles it as it compiles a module, with
   the same reload, errors, PSO records and cook. A material file names the graph where it
   names a module (`shader = asset("water.mgraph", …)`), its parameters by name as for a
   module's type.
2. **The file** is in the editor's grammar (1a): nodes by short random ids (two people's new
   nodes never collide), each with its kind, place and values; links keyed by the input they
   feed (an input has one link, so a link cannot be doubled); written sorted, so a diff
   shows what changed.
3. **Ports have kinds as Slang's types:** `float`, `float2`, `float3`, `float4`, a texture,
   with colours. Into a wider port a scalar fills every component; into a narrower one the
   extra components are dropped (Slang's truncation); a vector into a scalar takes `x`. An
   input with no link takes its default, shown on the node.
4. **The output node** holds the surface's terms of its shading model (base, normal,
   metallic, roughness, emission, alpha, occlusion, and the model's own: coat, sheen, toon's
   bands, rim…); the model is the output's choice.
5. **Code generation in three passes** (after three.js's stages): expand functions and
   reroutes; count each node's uses from the output back; write each node once, in place
   when used once, as a variable when used more. Only nodes the output reaches are written. Each written
   line remembers its node, so Slang's errors go back to the node that made them.
6. **Numbers and parameters** (3a): a number node, a colour or a texture can be a parameter
   (named: it is a row of the material); unnamed numbers are literals in the saved build.
   While the graph is open, every number is a parameter of an editing build, so dragging it
   compiles nothing; saving compiles the literals in.
7. **Generated Slang** (4a) is written to the local cache under the graph's id and a hash of
   its text, and into packs by the cook; never beside the graph.
8. **The canvas** is the editor's own (ADR 0030), its data apart from its view (egui-snarl's
   lesson), reused by E12's state machines: nodes placed on a plane with pan and zoom,
   links as curves, ports as dots in their kind's colour; Space at the pointer opens the
   node search (the palette's matcher), a link dropped in empty space opens it filtered to
   the ports it can feed; textures dropped from the dock become texture nodes; reroutes,
   frames, F to frame; every edit one undo step in the one history.

## What we do not take

- **Integer node ids** (Godot) and generated object ids in JSON (Unity): merges.
- **A vector averaged into a scalar** (Godot): it costs and surprises.
- **Apply** (Unreal) or **Save Asset** (Unity) before the scene shows an edit.
- **Static switches and keywords** multiplying shaders (CLAUDE.md §9): a graph is one type.
- **Per-node previews** before the scene is the preview (E10-4).

## Sources (checked 2026-10-28)

- Godot 4.7 documentation, "Using VisualShaders":
  https://docs.godotengine.org/en/stable/tutorials/shaders/visual_shaders.html (MIT; ideas
  only).
- Godot, `VisualShaderNode` class reference (port types):
  https://docs.godotengine.org/en/stable/classes/class_visualshadernode.html
- Unity Shader Graph, "Data Types" (dynamic vectors, truncation and promotion):
  https://docs.unity3d.com/Packages/com.unity.shadergraph@17.0/manual/Data-Types.html
- three.js, "Three.js Shading Language" and `NodeBuilder` (build stages):
  https://github.com/mrdoob/three.js/wiki/Three.js-Shading-Language,
  https://threejs.org/docs/pages/NodeBuilder.html (MIT; ideas only).
- Material Maker (MIT): https://github.com/RodZill4/material-maker; "Godot showcase:
  Material Maker", https://godotengine.org/article/godot-showcase-material-maker/
- egui-snarl (MIT): https://github.com/zakarumych/egui-snarl, https://docs.rs/egui-snarl/
- Unreal Engine 5.8 and Blender 5.2: as in the materials UX note's sources.
