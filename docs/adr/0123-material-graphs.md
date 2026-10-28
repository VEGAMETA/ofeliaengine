# 0123. Material graphs

- Status: **Proposed (2026-10-28).** Follows the team's answers to the materials UX note's
  questions (2026-10-22: one grammar for graph and function files, 1a; numbers
  parameters while a graph is open and compiled in at save, 3a, measured below; the
  generated Slang only in the cache and packs, 4a). What it adds (the file's shape, the
  node library's first set, the steps) is proposed; the questions at the end are the
  team's.
- Date: 2026-10-28
- Research: [`docs/research/material-graph.md`](../research/material-graph.md); UX:
  [`docs/research/ux/materials.md`](../research/ux/materials.md).
- Starts [ADR 0107](0107-editor.md)'s E10-3. Builds on [ADR 0092](0092-custom-materials.md)
  (material types) and [ADR 0120](0120-one-material-file.md) (the material file).

## Context

CLAUDE.md §9 asks for a material graph in the editor that generates Slang, beside
hand-written Slang. Everything a graph needs at run time exists: a material type is a
Slang module (ADR 0092), compiled off the frame, reloaded with the previous version kept,
its parameters reflected and edited by name (E10-1); a module's save is on screen in about
0.38 s on both GPUs. What is missing is the graph: its file, the Slang it generates, the
canvas it is edited on.

The UX note's scenario: "Material: New Graph" on a rock opens a graph wired with the rock's
own textures; Space, "normal"; a link dropped in empty space, "smoothstep"; the moss
texture dropped from the dock becomes a node; each edit on the rock in about 0.25 s.

## Options

### Where a graph lives

- **a. Its own file,** named by material files where they name a module
  (`shader = asset("moss.graph", …)`), so many material files share one graph as many
  share one module. **Chosen:** a graph is a material type, not a material.
- b. Inside the material file. Every instance would copy the graph, or one material would
  be the graph's and the rest its variants (E10-4's question, not this one's).

### The file's shape (in the editor's grammar, 1a)

- **a. Nodes keyed by short random ids, links keyed by the input they feed.** Two people's
  new nodes never take the same id; an input has one link, so a link cannot be doubled;
  sorted on write, a diff shows the nodes and links that changed. **Chosen.**
- b. Nodes as a list, links as a list of pairs (Godot's `.tres`): an index shifts when a
  node is removed, and two branches adding nodes take the same indices.

### Types between ports

- **a. Slang's own rules:** a scalar into a wider port fills every component; a vector
  into a narrower port loses its last components; a vector into a scalar is its `x`.
  **Chosen:** what the generated code does is what Slang does.
- b. Godot's: a vector into a scalar averages it. It costs instructions and differs from
  what the same Slang would do.

### Numbers on nodes (3a, measured)

- **a. While a graph is open its numbers are parameters of an editing build; saving
  compiles them in.** Chosen by the team (3a), after this measurement
  (`bench/graph-numbers/graph_numbers.slang`, `material_registers --module … --types …`,
  2026-10-28): a toon and a water graph's code with their numbers in it, then read from
  their parameters.

| Fragment shader registers, numbers compiled in → as parameters | plain | local lights | environment | lights + environment |
|---|---:|---:|---:|---:|
| RTX 4060 Ti (NVIDIA 617.42, Register Count): toon | 40 → 48 | 48 → 56 | 44 → 56 | 48 → 56 |
| RTX: water | 48 → 55 | 48 → 56 | 54 → 56 | 54 → 56 |
| Radeon iGPU (26.8.1, VGPRs): toon | 40 → 45 | 50 → 60 | 55 → 55 | 63 → 72 |
| Radeon: water | 35 → 43 | 54 → 54 | 54 → 54 | 73 → 74 |
| (the engine's glTF shading: RTX; Radeon) | 56; 47 | 63; 64 | 63; 64 | 56; 80 |

  Parameters cost up to +12 registers on the RTX and +10 VGPRs on the Radeon, where the
  toon's lit variant crosses 64 (63 → 72), as the dead code that cost the forward pass
  9–13 % did (CLAUDE.md §5).
- b. Always parameters: the shipped game pays the table above.
- c. Always compiled in (Unreal): a compile, about 0.25 s, behind every drag.

### Where the generator runs

- **a. One crate, `engine_material_graph`,** used by the player (a graph named by a
  material file is generated into the local cache and compiled as a module), the cook
  (into packs) and the editor (the canvas, the editing build). **Chosen.**
- b. The editor only, writing modules the player reads: the generated code would live
  beside the graph, against 4a.

## Decision

### The file: `.graph`

```text
[graph version=1 id=7d2e5c0b9a4f41e3b8c6d5a4f3e2d1c0]

# Moss on the tops of rocks.
model = "lit"
nodes["out"].kind = "output"
nodes["out"].at = [640, 120]
nodes["a3f9"].kind = "texture"
nodes["a3f9"].at = [80, 40]
nodes["a3f9"].param = "albedo"
nodes["a3f9"].texture = asset("rocks/rock_albedo.png", 1c2d3e4f5a6b4c7d8e9f0a1b2c3d4e5f)
nodes["b71c"].kind = "number"
nodes["b71c"].at = [80, 260]
nodes["b71c"].value = [0.6]
nodes["b71c"].param = "roughness"
links["out.base"] = "a3f9.rgba"
links["out.roughness"] = "b71c.value"
```

- **`model`:** the output's shading model (`lit`, `unlit`, `clearcoat`, `cloth`, `hair`,
  `toon`, `sheen`, `sheen_clearcoat`): its own terms are the output node's inputs.
- **Nodes** by id: `kind` (the node library's name), `at` (its place on the canvas),
  `value` (a number's or a vector's components), `param` (a parameter's name: a row of
  the material, its value kept by name in material files), `texture` (an image, by asset),
  `comment`.
- **Links** by the input they feed (`"<node>.<port>"`), the value the output they come
  from. An input without a link takes its default, the node's `inputs["<port>"]` when it
  was set.
- **Version 1;** a later version adds functions, frames and reroutes, as the steps below
  say.

### Code generation

- **One module a graph:** a `struct Graph_<id>Params` of its parameters (textures first,
  then vectors and numbers by name) and a `struct Graph_<id> : IMaterial` whose `surface`
  is the output's terms. A material file of a graph names the type its module declares,
  `GraphMaterial` (as built in E10-3c: the one type of every graph's module).
- **Three passes:** expand (functions and reroutes, later); count each node's uses from the
  output back; write each node once (in place when read once, a `let` when read more).
  Nodes the output does not reach are not written.
- **A line map:** each written line remembers its node, so Slang's errors go to the node
  that made them, and the canvas marks it.
- **The editing build** (3a): every number is a parameter, so dragging one compiles
  nothing; the saved build writes unnamed numbers as literals. Named parameters are
  parameters in both.
- **Where it is written** (4a): the local cache, `.ofelia/cache/graphs/<id>-<hash>.slang`,
  and by the cook into packs; Show Slang opens it read-only.

### The node library's first set

Output; Number, Vector 2/3/4, Color; Texture (sampled at the mesh's coordinates or at
given ones), Texture Coordinates, World Position, Normal, Tangent, View Direction (from the
frame's camera position); Add, Subtract, Multiply, Divide, One Minus, Negate, Abs, Floor,
Fraction, Min, Max, Clamp, Saturate, Power, Square Root, Sine, Cosine, Lerp, Step,
Smoothstep, Dot, Cross, Normalize, Length, Distance; Split, Combine; Normal Map (a tangent
space map to the world, with a strength); Fresnel. Each is a snippet of Slang with typed
ports (Material Maker's way), so a new node is a few lines.

### The canvas

The editor's own, its data apart from its view (reused by E12's state machines): in the
Shader panel under the viewport (the UX note's layout); pan with the middle or right
button, zoom with the wheel, F to frame; nodes dragged, boxed, deleted, duplicated; a link
dragged from a port to a port of a kind it can feed, Ctrl+drag yanking a port's links
(egui-snarl's); Space at the pointer opens the node search (the palette's matcher), a link
dropped in empty space opens it filtered; textures dropped from the dock become texture
nodes; every edit one undo step in the one history, the file written by Ctrl+S and
autosaved.

## Steps

- **E10-3a:** this ADR, the research note and the measurement.
- **E10-3b:** `engine_material_graph`: the file read and written, the node library, the
  generator and its line map; tests: every node's code compiles inside `forward.slang`,
  generated text against expected files, a round trip of the file.
- **E10-3c:** the player and the cook: a material file naming a graph draws it (generated
  into the cache, compiled, reloaded when the graph changes); a golden shot of a graph's
  material on both GPUs.
- **E10-3d:** the canvas: opening a `.graph` in the Shader panel, nodes, ports, links, pan,
  zoom, selection, moves, deletes, undo; "Material: New Graph" (wired with the selected
  object's own textures).
- **E10-3e:** the node search (Space, a link dropped in empty space), textures dropped from
  the dock, reroutes and frames (version 2).
- **E10-3f:** live edits: the editing build while open, compiled in at save; errors on
  nodes; Show Slang.
- **E10-3g:** functions (Ctrl+G, Tab, Ctrl+Alt+G), project Slang functions marked `[Node]`
  as nodes, an Expression node.

## Consequences

- A graph is one more way to write a material type: everything ADR 0092 gives a module
  (reload, errors kept, PSO records, packs, the register rule) it has.
- One crate generates the code everywhere, so the editor's, the player's and the cook's
  modules are the same text.
- The file reads in a diff, and two branches' new nodes merge.
- The cost of a graph is the cost of its code: unnamed numbers compiled in.

## As built (E10-3b-1, 2026-10-28): the node library and the generator

- **`engine_material_graph`,** a new crate with no dependency but the profiler's zones:
  - `library`: 40 node kinds and the surface, each a snippet of Slang with typed ports
    (`Float(1..=4)` or `Dynamic`), what an input takes unlinked (values, or an expression
    of the fragment such as `ctx.uv`), and whether it reads the fragment;
  - `graph`: a graph in memory as the file holds it (its model, nodes by id with their
    place, value, parameter name and inputs' own values, links by the input they feed);
  - `generate`: the module, with `Generated::node_at(line)` for Slang's errors.
- **The generator** as decided above, in detail:
  - each of `IMaterial`'s functions writes only the nodes its terms reach, each after
    those it reads; a loop is an error at a node in it;
  - a node's dynamic width is the narrowest vector linked to its dynamic inputs (a
    scalar where only scalars are); `dot`, `length`, `distance` and `normalize` of a
    scalar have their own forms;
  - conversions: a scalar fills a vector (`float3(x)`), a wider vector is cut
    (`(v).xy`), a narrower one gains zeros and a `w` of 1;
  - a value read twice in a function is a variable, once it is written in place; a
    texture's sample is always a variable;
  - the saved build writes unnamed numbers as literals, the editing build reads them as
    parameters named `_<node>`; named numbers and textures are parameters in both,
    textures first, then by name (a struct with none holds `float _unused`);
  - parameter names start with a letter, are not Slang's words, and are taken once;
  - the alpha cutoff (`alpha_cutoff(Params p)`, no fragment) refuses a node that reads
    the surface, at that node;
  - the normal map's function is a member of the type, so modules of several graphs can
    be read together.
- **Tests:** the parameters and the surface's lines; literals in the saved build and
  parameters in the editing one; a variable for a value read twice; conversions and
  dynamic widths; a dot of scalars; a model's terms after the common ones; errors at their
  nodes (a loop, a kind, a port or a node that is not there, a parameter named twice or
  wrongly, not a number, a second surface or none, a type name); the alpha cutoff; the
  normal map's function once. With Slang (`ENGINE_REQUIRE_SLANG` in the gate): 73 types,
  every kind with its dynamic inputs scalar and then vector, every model and the editing
  build, generated into one module and checked inside `forward.slang`; a misspelt
  function in a kind fails it at its line; the editing build's parameters as Slang lays
  them out (`albedo`, `_m`, `roughness`).
- **Next:** the `.graph` file (E10-3b-2, the shape proposed above); the player and the
  cook (E10-3c), where the material inspector also skips fields starting with `_` (the
  editing build's numbers, `_unused`).

## As built (E10-3b-2, 2026-10-28): the `.graph` file

Written in the shape this ADR proposes (its recommendation; cheap to change while no game
has graphs).

- **`engine_material_graph::file`:** `GraphFile` (the model by name, `nodes` by id,
  `links` by the input they feed) and `NodeFile` (`kind`, `at`, `value`, `param`,
  `texture`, `inputs`, `comment`), reflected and written by the editor's grammar
  (`engine_scene::write_value`, `read_value`) under the header `[graph version=1 id=…]`;
  `GraphFile::of(&graph)` and `graph()` go between the file and the graph in memory.
- **What is written:** a constant's components as wide as it is (`value = [0.6]`), an
  input's as wide as the input (a dynamic input's once when its components are the same:
  `inputs["a"] = [0.25]` fills them on reading); a field at its default (an empty
  parameter, no texture, no inputs, no comment, no value) is not written. The grammar
  writes maps sorted, so nodes come by id and links by their input.
- **A texture node's own image** (`Node::texture`, an asset reference written with its
  path): what the material shows until a material file gives the parameter one (the
  player's part, E10-3c). Nodes have a `comment` too.
- **The database** knows `.graph` files (`AssetKind::Graph`, their id inside, as
  `.material` and `.mixer` files); the FileSystem dock shows them with the shader icon
  until the canvas (E10-3d).
- **Tests:** a graph reads back what it writes (with its id and its texture's path); the
  text holds only what differs from the defaults, sorted, as the example above; a model
  or a link that cannot be read is said, a newer file and another kind of file are
  refused; the database's kind and id.

## As built (E10-3c, 2026-10-28): graphs drawn by the player

- **`engine_assets::graph_module`:** where a material file's `shader` is a `.graph`,
  `MaterialFile::typed` makes it a module: the graph read, generated in the saved build
  with the type `GraphMaterial`, written to `<project>/.ofelia/cache/graphs/<graph id>.slang`
  only when the text changed (an unchanged graph compiles nothing again; the folder is the
  editor's session folder, which new games' `.gitignore` leaves out). The resolved
  material's shader is that module, so the player, the editor's material document and the
  cook (which reads material files through `typed` as well) all compile the same text.
- **The graph's own values** are the material's defaults under the file's: its named
  numbers, and its texture nodes' images with an encoding inferred from the graph (a
  normal map's where the node feeds a Normal Map node, else a colour's; a material file's
  `textures["…"].encoding` says otherwise). `TypedMaterial::textures` names each texture
  parameter's image asset, the file's or the graph's.
- **The player** (`engine_app::Assets`) loads those images as a typed file's, and notes the
  graph's file among the material's dependencies: a graph written anew reads the material
  again, its module is generated anew, and the forward renderer's hot reload (which the
  editor turns on) compiles it again, the previous version drawing until then.
- **The editor's material rows** leave out fields starting with `_` (a graph's editing
  numbers, a placeholder field).
- **Measured** (the test's dev build, ref-1): a cube's graph drawn 0.71 s (RTX) and
  0.47 s (Radeon) after its first frame, its look meanwhile; the graph's edit on screen
  0.61 s and 0.49 s after it was written.
- **Tests:** a material file naming a graph resolved to its module under the graph's id,
  the type, the file's number over the graph's, the graph's colour and its texture (a
  normal map's) where the file gives none, not written again when unchanged, a graph that
  cannot be generated said with its node; on both GPUs, a cube drawing its look, then the
  graph's colour, then the colour the graph was rewritten with.
- **Not yet:** a cook test of a graph's material (the cook takes the same path); the
  canvas (E10-3d).

## Questions for the team

1. **The graph file's name.** A material graph is a file of its own, as a Slang module is.
   - a. `moss.graph`: short; E12's animation state machines take a name of their own.
   - b. `moss.materialgraph`: says what it is, at a length.
   - Recommendation: a.
2. **This design as a whole** (the file's shape: nodes by short random ids, links keyed by
   the input they feed; Slang's own type rules between ports; one crate generating the code
   for the editor, the player and the cook; the first nodes; the steps).
   - a. Accepted: E10-3b starts on it.
   - b. Changes first (say which).
   - Recommendation: a. Meanwhile E10-3b's code generator and node library, which do not
     depend on the file's shape, can start.
