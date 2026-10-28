# 0123. Material graphs

- Status: **Accepted (team decision, 2026-10-28):** the design as proposed (answer 2a);
  the file's extension `.mgraph` (the owner's own choice over `.graph` and
  `.materialgraph`); graphs open as files in tabs of their own, as code does, not in a
  panel under the viewport (the owner: "шейдеры лучше без нижней панели просто
  отдельными файлами"). It follows the team's answers to the materials UX note's
  questions (2026-10-22: one grammar for graph and function files, 1a; numbers
  parameters while a graph is open and compiled in at save, 3a, measured below; the
  generated Slang only in the cache and packs, 4a).
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
  (`shader = asset("moss.mgraph", …)`), so many material files share one graph as many
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

### The file: `.mgraph`

```text
[mgraph version=1 id=7d2e5c0b9a4f41e3b8c6d5a4f3e2d1c0]

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

The editor's own, its data apart from its view (reused by E12's state machines): a graph
opens in a tab of its own, as a code file does (the team's decision, 2026-10-28, over the
UX note's panel under the viewport); pan with the middle or right
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
- **E10-3d:** the canvas: opening a `.mgraph` in a tab of its own, nodes, ports, links,
  pan, zoom, selection, moves, deletes, undo; "Material: New Graph" (wired with the
  selected object's own textures).
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
- **Next:** the `.mgraph` file (E10-3b-2, the shape proposed above); the player and the
  cook (E10-3c), where the material inspector also skips fields starting with `_` (the
  editing build's numbers, `_unused`).

## As built (E10-3b-2, 2026-10-28): the `.mgraph` file

Written in the shape this ADR proposes (its recommendation; cheap to change while no game
has graphs).

- **`engine_material_graph::file`:** `GraphFile` (the model by name, `nodes` by id,
  `links` by the input they feed) and `NodeFile` (`kind`, `at`, `value`, `param`,
  `texture`, `inputs`, `comment`), reflected and written by the editor's grammar
  (`engine_scene::write_value`, `read_value`) under the header `[mgraph version=1 id=…]`;
  `GraphFile::of(&graph)` and `graph()` go between the file and the graph in memory.
- **What is written:** a constant's components as wide as it is (`value = [0.6]`), an
  input's as wide as the input (a dynamic input's once when its components are the same:
  `inputs["a"] = [0.25]` fills them on reading); a field at its default (an empty
  parameter, no texture, no inputs, no comment, no value) is not written. The grammar
  writes maps sorted, so nodes come by id and links by their input.
- **A texture node's own image** (`Node::texture`, an asset reference written with its
  path): what the material shows until a material file gives the parameter one (the
  player's part, E10-3c). Nodes have a `comment` too.
- **The database** knows `.mgraph` files (`AssetKind::Graph`, their id inside, as
  `.material` and `.mixer` files); the FileSystem dock shows them with the shader icon
  until the canvas (E10-3d).
- **Tests:** a graph reads back what it writes (with its id and its texture's path); the
  text holds only what differs from the defaults, sorted, as the example above; a model
  or a link that cannot be read is said, a newer file and another kind of file are
  refused; the database's kind and id.

## As built (E10-3c, 2026-10-28): graphs drawn by the player

- **`engine_assets::graph_module`:** where a material file's `shader` is a `.mgraph`,
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

## As built (E10-3d-1, 2026-10-28): the canvas in a tab of its own

- **`engine_editor::graph`:** the graph's document (`GraphDoc`: the file held, its text
  before and after each edit for the history, its view, its selection, its boxes laid
  out), and what the canvas does with the pointer (`GraphDoc::pointer`), apart from the
  drawing, so its rules are tested without a window.
- **The view:** a pan and a zoom by steps (25 % to 300 %, eleven of them, so the glyph
  atlas holds a few sizes of text); the wheel zooms about the pointer, the middle or the
  right button pans; F frames the selection or every node, as a graph opens.
- **Boxes:** a title (its parameter's name after its kind's), the inputs down the left,
  the outputs down the right, as wide as their texts; a constant's value (a colour's
  swatch) where inputs would be; the surface shows its model's inputs and any other
  linked. The title's colour is the node's group, a port's what it carries (scalar grey,
  2 green, 3 yellow, 4 pink, dynamic blue: Unreal's and Shader Graph's habit), their names
  saying it too. Links are cubic curves leaving and arriving level, those of the selection
  brighter.
- **The pointer:** from an output to an input or back, a link (an input's old one
  replaced); a linked input's end lifted, moved to another input or dropped on the
  canvas (taken out); a click selects (Shift toggles), a drag moves the selection and ends
  on whole units; a box on the empty canvas selects what it meets; Escape takes a move
  back. A link that would loop back is refused, said in the status bar.
- **Keys:** Delete, F, Escape on the canvas (the tree's and the dock's keys stay theirs
  after a press in them); Ctrl+S, Ctrl+W, Ctrl+Tab, undo; the scene's tools are off while
  a graph shows (the code tabs' rule).
- **History:** each edit a file step (`History::change_file`), its text put back by undo
  into its tab, or into the file opened again where the tab was closed.
- **Session:** the open graphs' tabs and the shown one come back; unsaved ones are
  autosaved as material files are (`.ofelia/autosave/<name>-<hash>.mgraph`).
- **Drawing:** under the panels (layer −1), after the viewport's overlays, which its
  background covers; nothing allocated in a steady frame (240 frames measured, the pointer
  going over boxes, ports and links).
- **Fixed with it:** Ctrl+Tab moved the panels' focus to their first field (the UI's focus
  order took the Tab), so the editor took it for typing and the tabs did not change.
- **Tests:** the view (zoom about the pointer, pans, framing), the layout (widths from the
  texts, the surface's inputs by model), hits (ports before boxes, the selected box on
  top), links and loops, the pointer's links, lifts, moves, clicks, boxes and Escape,
  deletes and undo, the file and the session read back; in the editor, a graph opened, its
  canvas framed, a link made by the mouse, undone and redone, a box moved, deleted with
  Delete (the scene's cube staying), the wheel and the middle button, F, Ctrl+S, Ctrl+W and
  an undo opening the closed graph again; on both GPUs, the canvas drawn (its title bars
  and links in their colours) and gone behind the scene's tab; and the zero allocations.
- **Next (E10-3d-2):** "Material: New Graph" (wired with the selected object's own
  textures), duplicates (Ctrl+D), Ctrl+drag yanking a port's links; then the node search
  (E10-3e).

## As built (E10-3d-2, 2026-10-28): New Graph, copies, carried links

- **`GltfOutline::maps`** (`engine_assets`): each glTF material's own look from the JSON
  alone. It holds the factors (base colour, metal, roughness, emission with its strength)
  and the maps' images where they are files beside the glTF (percent-decoded; one inside
  the file, a data URI or a bad index is none). The indices are checked, because the
  document is not validated.
- **`graph::starter`:** a lit graph drawing that look. The colour map is multiplied by its
  tint where the factor is not 1. The metal/rough map's blue and green channels are
  multiplied by their factors. The normal map goes through a Normal Map node, then come
  emission and occlusion. Factors stand alone where there is no map. Without a look, the
  graph is the standard material's (grey 0.8, roughness 0.5, no metal).
- **Material: New Graph** (`material.new_graph`): `<name>.mgraph` and `<name>.material`
  are named after the active object (numbered when taken) in the dock's folder.
  - The look is the object's model's glTF material, found by its part's name as a dropped
    model names it.
  - The material file names the graph (`GraphMaterial`), marks its metal/rough and
    occlusion maps as data (the graph's inference would take them for colours), and takes
    the look's factors as its look.
  - It is given to the selection in one step, and the graph opens in its tab.
- **Copies (Ctrl+D on the canvas):**
  - each copy gets an id of its own (`multiply1`), a parameter name of its own
    (`tint_2`) and sits 30 units down and right;
  - links among the copied nodes are copied between their copies, and links into them
    from the other nodes are kept (Blender's);
  - the surface is not copied, and the copies are selected.
- **Carried links (Ctrl+drag from an output, egui-snarl's yank):** every link of the
  output, dropped on another output, moves there; if one would loop, all are refused and
  stay. Dropped elsewhere, they are taken out. While carried, they are drawn from the
  pointer.
- **Tests:**
  - the outline's maps (factors, a file, an embedded image, a bad index, glTF's defaults);
  - the starter with and without a look (its links, a factor of 1 linking the channel
    itself, a missing file left out), its module generated and compiled by Slang inside
    `forward.slang` with every map and with none;
  - copies with their links and names, and a carried output moved, refused and taken
    out;
  - in the editor, New Graph on a box drawing a glTF material with two maps (the graph,
    the material file, one step, its tab, Ctrl+D there, the scene's box not copied), and
    again on the box, whose material is a file now (the standard look, numbered).
- **Next:** the node search (E10-3e).

## As built (E10-3e-1, 2026-10-28): the node search, links ending in it, dropped images

- **The search is the palette's** list (its fuzzy matcher, its keys), opened at the
  pointer (the palette learned a place, `PaletteView::at`) in a narrower box:
  - every kind but the surface, by group (`Math: Multiply`), its ports beside it
    (`a, b → out`);
  - the pick added where the pointer was (`GraphDoc::add_node`: an id of its own, on
    whole units, selected), one step.
- **Keys:** a key context of the canvas's own (`GRAPH_KEYS`, as the code tabs' and the
  dock's), Space and Blender's Shift+A in it; **Graph: Add Node…** in the palette opens it
  at the canvas's middle.
- **A link dropped on the empty canvas** asks for the search there
  (`Outcome::Search`):
  - it offers only the kinds that can take the link: with an input for an output's link,
    with an output for an input's;
  - the node added is linked by its port that fits best: one as wide, else a dynamic one,
    else its first.
- **An image dropped from the dock** onto the canvas (a drop target of its own,
  `DropTarget::Graph`) is a texture node of it at the pointer, its parameter named after
  the file (`Wood Grain.png`: `wood_grain`, numbered when taken), its path in the graph's
  references. Anything else is refused with why.
- **Tests:**
  - the kinds offered, the ports' text, nodes added plainly and linked both ways by the
    port that fits, the surface refused, the search asked for by a dropped link, images'
    nodes and their names;
  - in the editor: Space at the pointer, typed and Enter (the node where the pointer
    was, one step), a dropped link's search linking its pick, an image dragged from the
    dock onto the canvas, undone;
  - the editor's and the graph tab's frames still allocate nothing.
- **Next (E10-3e-2):** reroutes and frames (the file's version 2).

## As built (E10-3f-1, 2026-10-28): values in the inspector

Taken before E10-3e-2 (reroutes and frames): without it a graph's values could only be
written by hand.

- **Rows** (`graph::NodeRow`): what the inspector shows while a graph's tab is shown.
  - With no node selected alone, the graph's shading model.
  - For a node: its parameter's name, a constant's value (a colour's channels), its
    unlinked inputs' own values (an input whose kind fills it with a value; a dynamic one
    shows one number and fills every component with it), a texture's image, and its
    note. The surface shows only its model's inputs.
- **Texts:** made every frame into the same room (the zero-allocation tests unchanged).
- **Edits:** each is a file step of the one history; a drag's moves merge into one.
  - Numbers: dragged with the editor's snapping, typed with its expressions.
  - Names: a parameter's name must be a Slang field's and no other node's, else refused
    with why.
  - The model: by its list.
  - Reset (↺, Backspace): back to the kind's own (no parameter, its input's value, no
    image, no note, the lit model).
  - The image: picked in the palette (`#texture:`), or dropped from the dock onto its row.
- **Tests:**
  - every row kind, texts, edits, refusals and resets;
  - in the editor: the model by its list, the colour clicked on the canvas, its red typed,
    its green dragged as one step, an image dropped from the dock onto a texture node's
    row, undo, and the file read back as saved.
- **Next (the team's answer, below):** the material's preview while its graph is shown.

## As built (E10-3f-2, 2026-10-28): the preview sphere

The team's «а» (answer 4 below).

- **What draws it:** the graph's editing build (`Build::Editing`), every number a
  parameter, so a value's edit changes no code.
  - Its module is written to `.ofelia/cache/graphs/<graph id>-editing.slang` when its
    text changes; the player's material hot reload compiles it.
  - Its material is a file the editor holds (`Player::set_material_file`, with ids of its
    own made from the graph's), never written to disk.
  - A sphere of it is asked for as a preview (ADR 0112) and shown, 128 points square, in
    the canvas's top right corner (an image over the canvas; the pointer over it is not
    the canvas's).
- **After every edit:** the inspector's and the canvas's edits of a frame reach the
  preview's request in that frame.
  - A value: the module's text unchanged, the request says so
    (`PreviewRequest::same_code`) and is drawn once, as soon as no material type is on
    its way.
  - A change of code: the module written; the request draws twice, as a type's first
    preview does (its pipelines asked for, then drawn once they are made).
  - Refused (a parameter the old layout lacks fails the material until its type is
    compiled again): asked for again once the material can be made. Why, in the bar,
    only while no type is on its way.
- **Found and fixed on the way** (`engine_render`, `engine_app`):
  - a reload due (a file changed, its 30 ms of quiet not over) did not count as a type on
    its way: a preview's second drawing could be made with the old code, and kept;
  - the pipelines on their way were counted after the view's prepare, which takes layouts
    that arrived after this frame's materials were laid out: counted before it now;
  - a drawing whose code is unchanged is answered only when its frame asked for no
    pipeline; otherwise it is drawn again once they are made;
  - a request waiting (its parts not made, or types on their way) allocated its draws
    every frame: they are made only for the drawing now, so waiting allocates nothing.
- **Measured** (the editor's GPU test, 1280 × 720 offscreen; Radeon / RTX, the spread of
  runs with the desktop in use):
  - the first sphere after the tab: 0.70–0.88 s / 0.66–0.81 s (the module compiled);
  - a number typed: 58–60 ms / 34–36 ms, 5 frames (with two frames in flight, the
    least);
  - a link bringing a new parameter: 0.60–0.77 s / 0.56–0.69 s. On the Radeon about
    340 ms go to compiling the type's entry points again, then about 180 ms to
    reflecting its parameters in a second compile (status §6);
  - the editor's and the graph tab's frames still allocate nothing (the graph's test
    now measures once its types are made).
- **Tests:**
  - a value's edit changes no code (unit);
  - a request whose code is unchanged answered after one drawing (the player's typed
    material test: 3 frames);
  - in the editor on the GPU: the sphere drawn, a typed number and a link with a new
    parameter reaching it.
- **Next:** errors on nodes and Show Slang (E10-3f-3), then E10-3e-2.

## As built (E10-3f-3, 2026-10-28): errors on nodes, Show Slang

- **Errors on nodes:** the shown graph's generation (its editing build, made after each
  edit for the preview) keeps what stopped it (`GraphDoc::error`, at its node when it has
  one).
  - The canvas frames that node in red, with why under the box.
  - The bar says why ("not drawn: node a: …"); the preview's sphere keeps its last
    drawing.
  - What reaches it: what the canvas cannot make but a file can hold, such as a loop a
    merge left, or a kind or a port that is not there (a newer engine's file).
  - Slang's own errors at their nodes (the line map, `Generated::node_at`) wait for
    E10-3g: the generator's code compiles (73 types checked in the gate), so nothing
    could reach them before nodes whose code the user writes.
- **Show Slang:** the bar's button and the palette's `graph.show_slang`.
  - What it opens: the saved build (what the player and the cook write, unnamed numbers
    as literals), of the graph as its tab has it, in a code tab at `<graph>.slang`,
    never on disk. The Slang highlighting, outline and checker are a code tab's.
  - Read only (`code::Document::set_read_only`): every edit refused at the one place
    edits go through, and said once; a cut copies.
  - Following the graph: made again where the graph's revision moved, while its tab is
    open (`Document::show_text`: no undo step, the caret kept where the text has room).
  - Ctrl+S writes nothing; the session and autosaves leave it out.
- **Tests:**
  - a loop in a file: the error at one of its nodes and in the bar, gone with the link;
  - the read only document: edits refused and told, a cut that copies, a new text
    without a step;
  - Show Slang: the tab, its label and the saved build's numbers; typing refused and
    told; Ctrl+S writing nothing; a value changed in the graph followed; the session
    without it.
  - The editor's and the graph tab's frames still allocate nothing.
- **Next:** reroutes and frames (E10-3e-2, the file's version 2), then functions
  (E10-3g), with Slang's errors at their nodes.

## As built (E10-3e-2a, 2026-10-28): reroutes

Studied for this and E10-3e-2b (2026-10-28):
- Unreal's material editor puts a reroute on a wire double-clicked, and wraps the
  selection in a comment box by C; the box moves the nodes inside it, and a node dragged
  in or out joins or leaves
  (dev.epicgames.com, "Organizing a Material Graph in Unreal Engine").
- Godot 4.3 attaches nodes to a frame explicitly (`attach_node_to_frame`), the frame
  fitting itself around them (`VisualShaderNodeFrame`).
- Blender joins the selection in a new frame (Ctrl+J in 4.1, F in 4.5) and adds a node
  to one by Ctrl+P or by dropping it on it; Shift and the right button swept across a
  link add a reroute (manual 4.1, "Frame Node").

What was built:
- **A node kind** (`library::REROUTE_KIND`, category Layout): one dynamic input `in`, one
  output `out`, its body `{in}`. Written in place, it adds nothing to the code: a graph
  through a chain of reroutes generates the module it does without, in both builds
  (tested). No new file version: a reroute is a node as others are.
- **On the canvas:** a capsule of 28 by 14 units in the colour of what it carries,
  followed back through a chain of reroutes to the first node that is not one
  (`GraphDoc::carried_by`, no allocation); its pins at its ends, reaching half as far as
  a node's ports so that its middle can be grabbed to move it; no title.
- **Added** by a double-click on a link (two presses within 400 ms and 3 pixels, the
  link within 60 % of a port's reach of its curve), the link passing through it, it
  selected alone, one step; or from the node search.
- **Tests:**
  - the generated module unchanged by a chain of two reroutes;
  - a double-click on a link (a press and its release, then a second press) adding one
    in the link's place, laid out as a capsule centred there, its middle and its pins
    hit, and what it carries;
  - the graph tab's frames allocate nothing with a reroute shown, and the GPU test's
    graph draws one.
- **Next:** frames (E10-3e-2b, the file's version 2).

## As built (E10-3e-2b, 2026-10-28): frames, the file's version 2

- **The decision:** Unreal's comment boxes (the study is in E10-3e-2a's as-built).
  - A frame holds the nodes whose boxes lie wholly inside it when its drag starts: no list
    in the file, so two branches' edits of the same frame merge as places do.
  - Godot's and Blender's lists of attached nodes would keep a node in a frame it was
    dragged out of until detached, and add a field per node to merge.
- **The data:** `Graph::frames` by id (`Frame`: `at`, `size`, `title`), layout only, the
  generated code the same with or without them (tested).
- **The file's version 2:** `frames["frame1"].at`, `.size` and `.title` after the links.
  Version 1 files, which have none, read as they are; the editor writes version 2.
- **On the canvas** (`graph`):
  - Drawing: under the links and the boxes, an area the grid shows through, a title bar
    with the title, two strokes as a grip at the bottom right corner; selected, framed in
    orange, under the pointer, faintly.
  - Hits: the boxes stay on top; under them a frame's grip and title bar (the last frame
    on top), its inside the canvas's, so a box selection or a pan starts there.
  - Gestures:
    - **C** frames the selection: the boxes' extent with 20 units round it and the title
      bar above, titled "Frame", selected alone (refused with nothing selected).
    - The title bar drags the frame with the nodes it holds (`Drag::Move` carries
      frames).
    - The grip resizes it, to at least 80 by 50 (`Drag::Size`).
    - Escape takes either back; each is one step.
  - Selection: frames are selected apart from the nodes (`GraphDoc::selected_frames`),
    since a file could give a node and a frame the same id. Delete removes the selected
    frames and leaves their nodes.
  - The inspector shows a frame selected alone as "Frame · frame1" with its title (↺
    puts back "Frame").
- **Tests:**
  - the file: frames written and read back, a version 1 file read, the code unchanged;
  - the canvas: C (refused with nothing selected), the frame's place and size round the
    node, the title bar dragging it with the node inside and not the one outside, the grip
    resizing it, its inside the canvas's, the title in the inspector, Delete leaving the
    node;
  - in the editor: C as a step, undone and redone, the file written as version 2 with
    the frame;
  - the graph tab's frames allocate nothing with a frame shown, and the GPU test's graph
    draws one.
- **Next:** functions (E10-3g), with Slang's errors at their nodes.

## As built (E10-3g-1, 2026-10-28): the Expression node, Slang's errors at their nodes

- **The node** (`EXPRESSION_KIND`, `Body::Expression`, in the Code group): Godot's
  Expression node.
  - Inputs `a` to `d` (four floats, unlinked zero) and an output `out` (four floats), the
    code in `Node::code` (the file's `nodes["e"].code`).
  - The generator writes it as a static member of the type,
    `expression_<id>(MaterialContext ctx, float4 a, float4 b, float4 c, float4 d)`:
    - code with `return` is written as it is;
    - code without it is an expression, its lines joined and returned;
    - empty code is `a`.
  - A use calls the function with its inputs. A use read more than once in a function is
    a variable, as every node's value is.
  - Every line of the function is mapped to the node in the generator's line map (E10-3f-3),
    so Slang's errors there are the node's.
- **Slang's errors at their nodes** (`graph::slang_error`):
  - The preview's refusal carries the material's problem: Slang's diagnostics as the
    pinned Slang (2026.18) writes them, `error[E30015]: title`, then `--> path:line:col`,
    the source line and carets with the detail.
  - The first error placed in the preview's module is read: the node its line maps to, and
    the detail (or the title without one). Warnings, and errors placed in other files, are
    passed over.
  - It becomes the graph's error, so the canvas frames that node in red with the message
    under it (E10-3f-3's drawing) and the bar says "not drawn: node …". An error at a line
    of no node goes to the bar alone.
  - It is read once per refusal. It stays while the code does, through a node dragged or a
    value typed (the preview, refused again, would otherwise make it flicker). It goes when
    the code changes, or when a sphere is drawn.
- **Two fixes on the way** (found by the GPU test):
  - **A module written by the editor is due at once.** The editor wrote the module and the
    player's watcher saw it a moment later. Until then nothing was pending, so a refusal
    read the previous module's error. Now `Player::material_source_written` makes the
    reload due when the editor writes (`MaterialTypes::changed`, as a watcher's event
    does). The watcher's own event within the 30 ms quiet merges with it.
  - **A type broken from the start is reflected again once fixed.** A type whose module
    never compiled has no materials made, so it is never drawn and has no entry points. A
    reload with no entry points went idle without reflecting the parameters again, so a
    fixed module stayed unmade until a restart. This hit hand-written types (E10-2) as
    well. The reload now reflects the parameters all the same. A frame test (the type
    never drawn, fixed, reflected again in 0.3 s) fails without the fix.
- **Tests:**
  - the generator: the function's header, the expression returned, the body kept, its
    lines the node's, one call when read twice;
  - Slang: an error in an Expression's code is at a line the map gives the node;
  - the parser: Slang 2026.18's diagnostics with a warning in the module and an error in
    `forward.slang` before the module's error (the node and the detail); another file;
    a line of no node with the title;
  - the inspector's Code row, `set_code`, the title's first line shortened;
  - `MaterialTypes::changed`: nothing with hot reload off, the type of the file due, not
    the other, a compiling one compiled again after;
  - the editor on the GPU, end to end on both GPUs: the error framed at its node in
    0.38 s on the Radeon and 0.40 s on the RTX, kept on every frame of a drag; the code
    typed right in the inspector, then the frame gone and the sphere drawn in 0.97 s and
    0.91 s.
- **Next:** functions (E10-3g-2), their file's extension to be chosen.

## As built (E10-3g-3a, 2026-10-29): `[Node]` functions in the engine

- **The mark** (`engine_render/shaders/nodes.slang`): `[Node("Swirl", "Mapping")]` before a
  function of a project's Slang module.
  - Slang wants both arguments, the struct's two fields: a mark without them does not
    compile (checked with slangc 2026.18.3).
  - A graph's module includes the file before the modules its calls name. A module also
    used by a hand-written type includes it itself (`#pragma once`).
- **Finding the functions** (`engine_shader::marked_names`, `Compiler::marked_functions`):
  - Slang's flat API finds a function by name, but lists a module's declarations only
    through `IModule::getModuleReflection`, a COM interface ADR 0008's binding does not
    reach.
  - So the names come from the source: the functions declared after a mark, with comments
    skipped. The reflection then confirms the mark (`FindUserAttributeByName`) and reads
    the signature: the parameters with their types and directions, the result, and the
    mark's string arguments.
  - It runs Slang's front end only: 14 ms for a small module.
- **A call in the graph** (`engine_material_graph::call`): a node of kind `call`, with its
  `function`, its `module` (an asset) and its `signature` as Slang spells it.
  - The signature is kept in the file, as Unreal keeps a material function call's inputs
    and outputs in the material. The cook and the player generate without reflecting, and
    the editor writes it again when the function changes (E10-3g-3b). A stale signature
    fails at the call's line.
  - Ports: a first `MaterialContext` is the fragment's. `in` parameters are inputs (zero
    when unlinked), `out` ones outputs, `inout` both, and the result is `out`. Every port
    is float to float4, the graph's types; other types are refused with why.
  - Its kind is made once per function and signature and kept for the process
    (`call_kind`). It is `'static`, as the library's kinds are, so every reader of kinds
    takes both alike; what is kept grows with the different signatures a process sees.
    Owned kinds would change the 90 places that read `&'static Kind`, for no run-time gain.
- **Generation** (`generate_with`):
  - The modules the written calls name are included after `nodes.slang`. Each include line
    is mapped to its first call, so a module that is not found is that node's error.
  - A call's `out` and `inout` parameters and its result are variables; a `void` call is a
    statement.
  - `graph_module` (the cook and the player) and the editor's preview include a module by
    its path in the graph's file, from the cache's folder (`../../../shaders/mapping.slang`),
    so the text is the same on every machine.
- **The file's version 3** adds the call's three fields. Versions 1 and 2 read.
- **Tests:**
  - the scan: marks with arguments, other attributes and line breaks between are found;
    comments, structs, longer names and repeats are passed over;
  - Slang's reflection of marked functions: the mark's arguments, the parameters' types
    (`MaterialContext` and `TextureRef` included) and directions; unmarked, commented and
    missing names passed over;
  - signatures parsed and written back, refusals with why; a call's kind's ports, and its
    being made once;
  - the generator: `out` and `inout` variables, `void` statements, each module included
    once at its first call's line, a missing module at its node; the file's round trip;
    the include path;
  - with Slang: a graph calling a project module's function compiles inside
    `forward.slang` in both builds, and Slang's reflection gives back the signature the
    graph keeps.
- **Next:** E10-3g-3b, the editor's side.
  - The project's `[Node]` functions are found off the frame and listed in the node search
    under their groups, their nodes titled by the mark.
  - A call's signature is written again when its module changes.
  - Slang's errors in a module are shown at the calls.

## As built (E10-3g-3b, 2026-10-29): the editor's `[Node]` functions

- **Scanning** (`node_functions`): a thread of its own with its own compiler. The code
  tabs' checker would do, but it starts Slang's language server too.
  - It starts with the first graph shown. Every Slang module of the asset database is given
    to it then, again when the database is opened anew, and a module again when it is
    written anew.
  - A module without `[Node` marks costs a read. One with marks is checked inside
    `forward.slang` through a wrapper in `.ofelia/check/` (the mark's declaration, the
    module, an empty graph's type), and its functions are read by
    `Compiler::marked_functions`: 0.32 s for the first module, Slang's session made, the
    front end only.
  - Each function found has its mark's title and group (its name and "Functions" without
    them) and its signature in a call's spelling, or why it cannot be a node: a port that
    is not float to float4, a returned int, nothing given back.
- **The node search** lists them after the library's kinds, as "Group: Title" with their
  ports (`color, amount → out`), filtered as kinds are for a link drawn out of a port.
  - One a node cannot call is listed with why, and its pick says so.
  - A pick adds a call where the search was opened, linked as any node is, its module's
    path written into the file as an asset, as one step ("Add Call").
- **Titles** are the marks' (`GraphDoc::set_call_titles`), laid out again without changing
  any code; the function's name until the mark is known.
- **Signatures follow their functions** (`GraphDoc::refresh_calls`). When a scan finds a
  function whose signature differs from a call's, the call's is made the function's.
  - There is no step for it: the signature is the function's, and Unreal's calls follow
    their functions on load the same way.
  - The shown graph is checked every frame (an undo may bring back an older signature),
    with nothing allocated.
- **Errors in a called module** go to its first call (`slang_error`'s calls: the module's
  file name and the node).
- **Tests:**
  - a marked function's node, and its refusals (an int, a texture, a returned int, nothing
    given back, a context not first);
  - a call added as a step with its module's path, titled by its name and then its mark,
    its signature following its function without a step, and a function no node can call
    leaving it as it is;
  - Slang's error in a called module at the call;
  - in the editor, with Slang: the module's functions found in 0.32 s, listed under their
    group with their ports, a pick adding a call titled by its mark, and the one a node
    cannot call refused with why;
  - on both GPUs: a call whose kept signature is behind its function made the function's
    after the scan, the preview drawing what the function gives (1.18 s on the Radeon and
    1.63 s on the RTX after the tab).
- **Not done:**
  - a deleted module's functions stay in the search until the editor restarts (a pick of
    one then fails at its node);
  - parameter types other than float to float4.
- **Next:** functions (E10-3g-2), their file's extension asked.

## The team's answers (2026-10-28)

1. **The graph file's name:** `.mgraph` (the owner's own choice over the two proposed,
   `.graph` and `.materialgraph`), its header `[mgraph version=1 id=…]`.
2. **The design as a whole:** accepted (a).
3. **Where a graph opens** (the owner's note with the answers): as a file, in a tab of its
   own, as code does; no panel under the viewport for shaders or graphs. The bottom bar's
   Shader placeholder goes. This replaces the materials UX note's sixth point (shader
   code and graphs in a panel under the viewport).
4. **The preview while a graph is shown** (asked with E10-3f-1; the 3D view is behind the
   canvas): a sphere in the canvas's corner drawing the graph as it is, its numbers live
   (the owner's «а», over the canvas and the viewport side by side, or switching tabs).
