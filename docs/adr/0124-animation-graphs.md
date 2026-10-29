# ADR 0124: Animation in scenes, graphs, and the editor's animation tools (E12)

- Status: **Accepted (team decision, 2026-10-29):** every question at the end answered as
  recommended: 1c (an `Animator` plays a clip, a cross-fade or a graph), 2a (decisions in
  ticks, poses drawn every frame), 3c (cross-fade or inertialization per transition,
  inertialization when interrupted), 4a (sync by the cycle's fraction now, markers with
  the timeline), 5b (joints, events and number fields of components), 6a (one entity per
  skinned model, its joints in its tree), 7a (imported clips' settings in the model's
  `.meta`, `.anim` and `.agraph` files), 8a (no record mode) and 9b (the last 10 s of
  states and inputs kept while the editor plays, in E12-6).
- Research: [the runtime of state machines and blend graphs](../research/animation-graphs.md)
  (Godot, Bevy, Fyrox and ozz-animation read as code; Unreal and Unity from their
  documentation) and [the animation tools' UX](../research/ux/animation.md) (Godot, Unity,
  Unreal, Blender, Figma).
- Builds on [ADR 0068](0068-skeletal-animation-and-skinning.md) (skeletons, compressed clips,
  `Blender`, root motion, IK, skinning), [ADR 0107](0107-editor.md) §7 (phase E12),
  [ADR 0123](0123-material-graphs.md) (the graph canvas and the file grammar),
  [ADR 0019](0019-networking-model.md) (ticks), [ADR 0104](0104-save-games.md) (saves) and
  [ADR 0114](0114-luau-api.md) (scripts).

## Context

- `engine_anim` has every step a character needs: stateless sampling of compressed clips
  (about 0.3 µs for a 25-joint pose), a `Blender` of weighted layers with per-joint masks and
  additive layers, root motion, two-bone IK, FABRIK and look-at, and characters on the job
  system (1,000 Foxes in 0.13 ms on 16 threads, no allocation).
- Nothing plays a clip by itself. A game poses its skins in `Game::animate` and sets each
  `SkinnedMesh`'s palette. A scene's `Model` with a skin is drawn as bound: the player reads
  no joints or weights. A dropped glTF makes an entity per node, its skin's joints included.
  There is no animator component, no state machine, no event, and the editor's Animation
  tab is empty.
- ADR 0107's E12 lists: clip preview, the skeleton, the timeline and keys, state machines,
  blending.
- The engine's rules apply: speed first, no allocation per frame, data-oriented, text source
  files that merge, stable ids, nothing paid where unused (CLAUDE.md §0, §5, §13).

## Options

How a character's animation is driven:

- **a. An `Animator` component naming a graph file** (state machines, blends, layers),
  evaluated by the player. Transitions are data: seen, checked and debugged in the editor.
- **b. Clips played from code only** (Animancer's way in Unity): the component plays,
  cross-fades and layers clips; no graph file. The least to build now; every transition is
  code, seen only by running it, and E12's state machines are not there.
- **c. Both:** the component plays a clip or a cross-fade by itself (a door, a simple NPC)
  and runs a graph where one is named (a character).

How a graph is evaluated (from the code read):

- **Godot's flat per-track mix:** no intermediate poses, but rest-relative successive slerps
  depend on order, Variants and hash-map track caches cost on every track, and a node that
  needs a pose (IK, a model-space blend) cannot sit inside.
- **Bevy's per-bone evaluation:** a hash lookup and a boxed evaluator per property per bone
  entity, where our skeletons are flat arrays done in one pass.
- **Fyrox's:** every state's tree evaluated each frame, hash-map poses, parameters by
  string.
- **A graph compiled to flat arrays** and run as a short list of steps over a pose stack,
  `engine_anim`'s `Blender` doing the blends: what fits our rules.

## Decision (proposed)

### Characters in scenes

- A skinned model drawn skinned: the player makes the skin and a skinned instance from the
  model's glTF and poses it (its rest pose until something plays). Its clips load with it,
  compressed in the background and kept in the derived data cache (as E4-8 does for
  textures), cooked into packs as today.
- The joints are not entities (question 6): one entity holds the skeleton; the inspector
  lists the joints in a tree, the viewport draws and picks them, and an attachment is a
  child entity naming a joint (`JointAttachment`).

### The animator

- **`Animator`** (reflected, in `engine_runtime`): a graph (`AssetRef`, optional), a clip
  (played when there is no graph), speed, paused, and 16 parameter slots (numbers, flags and
  trigger counts, named by the graph). Scripts, the inspector, saves and replication see it as
  any component.
- **Its state:** one fixed-size block per character, laid out by its graph at spawn: per state
  machine its state, transition and times; per clip its phase now and at the last tick; per
  sync group its leader and phase; per blend space its smoothed input; the event ring;
  inertialization's history only where used. Kept in `engine_anim`'s pool, named by index
  from the component (as `SkinnedMesh` names its instance), written by the save provider.

### When it runs: decisions in ticks, poses in frames (question 2)

- **Each tick**, after the scripts' `on_tick` and before `Game::tick`: triggers consumed,
  transitions taken, phases advanced, the tick's events collected, its root motion computed
  for the character controller. The same at any frame rate and on every machine; saved and
  replicated with the rest of the simulation.
- **Each rendered frame:** the pose at the frame's instant between the last two ticks (as
  transforms are drawn): active clips sampled at the lerp of their unwrapped phases, weights
  lerped alike, then root motion removed, local to model, the game's IK (`Game::animate`
  stays for it), skinning matrices. A character not drawn skips its pose; a server, or a hit
  query, evaluates at the tick itself.

### The graph file and its evaluation

- **`.agraph`** (question 7), in ADR 0123's grammar: states, nodes and transitions keyed by
  random ids, names as labels, conditions as text, sorted on write. A sketch is in the
  research note.
- **Contents:** parameters; an ordered list of layers (normal or additive, a weight, a mask
  of joints with weights for their subtrees), each rooted at a state machine or a blend tree;
  states holding a clip, a 1D or 2D blend, or a machine; one-shot nodes fired by triggers;
  sync groups.
- **Cooked** into an immutable `engine_anim::AnimGraph`: flat `u16`-indexed arrays (nodes in
  post order, transitions sorted by source and priority, conditions as bytecode, blend-space
  points and triangles), bound once per skeleton (joint names to indices, masks to per-joint
  weights) and shared by every character.
- **A frame** runs only the nodes the tick left active, as steps on a pose stack:
  `sample(clip, phase)`, `blend(k)` (one `Blender::blend` over k slots, with weights, masks
  and modes), `inertialize`, `output`. The stack's depth is known at cook; scratch poses come
  from per-worker arenas. No hash maps, strings, Variants or trait objects on the way.
  Characters run in parallel (`for_each_parallel`); their events and field writes go to
  per-chunk buffers merged in entity order, so results never depend on threads.

### Transitions (question 3)

- **Conditions** are expressions over the parameters (`speed > 0.1 and grounded`), compiled
  at cook and checked as they are typed in the editor (a misspelt name underlined); a trigger
  is consumed by the transition that uses it. No script runs inside the decision.
- **Choice:** transitions from Any first, then the state's own, smallest priority first; at
  most 4 chained in a tick. Exit time in normalized time; an offset into the destination;
  "keep phase" starts it at the source's normalized phase.
- **Blends:** a cross-fade (both play, a curve per transition) or an inertialization (only
  the destination plays, the old pose's offset decays; half the sampling). An interrupted
  transition goes on as an inertialization from the current pose: no frozen pose, no growing
  stack of fading sources.

### Phase sync, blend spaces, events, root motion, fields

- **Sync (question 4):** groups by name, every blend space one: the heaviest member leads,
  the group's phase advances by `dt / Σ wᵢ·lengthᵢ`, every member samples at its fraction.
  Markers (left foot down…) later, with the timeline.
- **Blend spaces:** 1D by binary search; 2D Delaunay at cook (our own Bowyer–Watson),
  barycentric weights, the nearest hull edge outside, the triangle found by walking from the
  last one; input smoothed by a critically damped spring.
- **Events:** point events and windows on clips, fired over the tick's played interval with
  any number of loops and backwards; by default the sync leader or the heaviest sample fires,
  so a blend never doubles a footstep. Records go into a fixed-capacity ring: Rust reads them
  after the step, scripts get `on_animation_event` in the next tick's script stage. A window
  is state (`in_window("hit")`), so gameplay reads what saves and replicates.
- **Root motion:** per active clip over the tick's interval, blended by direction and length
  (ozz's way), summed for `Game::tick`'s controller or applied to the transform.
- **Fields (question 5):** a clip may animate number fields of components by `Tween`'s
  `Component.field` paths (a light's intensity, a material parameter, a field of view),
  bound at spawn (16 bytes a track), blended like joints, written once a tick.

### Where the editor's animation data lives (question 7)

- An imported clip's loop, root motion settings and events live in its model's `.meta`, so a
  new export from Blender keeps them; clips made in the editor are `.anim` files; graphs are
  `.agraph` files. Clips are parts of their model (`hero.glb#Run`), found as meshes are.

### The editor's tools

From the UX note's list:

- clips in the dock and the inspector with the engine's preview (the floor moving under root
  motion, the loop's seam as a number, two clips side by side, Auto Play);
- P plays the selected characters in place, writing nothing;
- joints in the viewport, Tab's joint mode, the joint tree, attachments by drop;
- the Animation tab (Alt+N): the dope sheet and curves, events (E adds one), a picked joint's
  trail and speed curve to find pops, Mark Foot Plants;
- keys without a record mode (question 8);
- graphs on E10's canvas in their tabs, blends in the inspector with Place by Speed, layers
  and masks, the live state in Play.

### Scripts (question 1)

The public scripting API is the team's (CLAUDE.md §3). Proposed calls on the component:
`animator:play(clip, fade)`, `animator:set(name, value)`, `animator:trigger(name)`,
`animator.state`, `animator:in_window(name)`, and the hook `on_animation_event(name, data)`.
Names are looked up once and cached, as tweens do.

## Steps

| Step | What | Needs |
|---|---|---|
| **E12-1a** | skinned models drawn skinned by the player; their skeleton and clips loaded with them, compressed in the background through the derived data cache | nothing open |
| **E12-1b** | clips seen: the dock's and the inspector's Clips with the engine's preview; P in place | nothing open |
| **E12-1c** | one entity per skinned model; joints in the viewport and the tree; attachments | question 6 |
| **E12-2** | the `Animator`: a clip, a cross-fade, layers, root motion; decisions in ticks; its state saved; the scripts' calls | questions 1, 2 |
| **E12-3** | the Animation tab: clips scrubbed and stepped, trails, events with sounds, Mark Foot Plants, loop and root motion settings | questions 7 |
| **E12-4** | keys: tracks of fields, the dope sheet's editing, curves, `.anim` files | questions 5, 8 |
| **E12-5** | `.agraph` graphs on E10's canvas: states, transitions, blends, layers, sync, the live state | questions 3, 4, 7 |
| **E12-6** | later: the last seconds of play kept (question 9), ghosts, IK in graphs, morph targets, retargeting | |

Measured as they land: the preview's frame cost on both GPUs; a graph's cost per character
beside ADR 0068's 0.13 ms for 1,000 Foxes (100, 1,000 and 10,000 characters, steady, in
cross-fades and in inertialized transitions, the tick part and the frame part apart); no
allocation with transitions and events every tick; the same events at the same ticks at 30,
60, 144 and 240 fps; a save and load in the middle of a transition; the panels within ADR
0107's 1 ms of UI.

## As built (E12-1a, 2026-10-29): skinned models drawn skinned

- **`engine_app::skins`:**
  - `skinned_node` finds the node that draws a mesh with a skin; the first in the file
    wins.
  - `build` makes the mesh's parts (the whole mesh, or a part per primitive with material
    slots) with their influences, padded to the most sets a primitive has, and the `Rig`.
  - The rig holds the skeleton (`skin_skeleton`), the skin joints' skeleton joints, the
    inverse bind matrices times `S⁻¹`, the root `S · node⁻¹` (`node_world` walks the
    file's parents; `node_parents` is now public in `engine_assets`), the rest pose's
    palette, and the file's clips compressed within ADR 0068's default error, each with
    its name, bytes and largest error.
  - `Posing` samples a clip at a moment into scratch sized for the largest rig.
- **The loader:** `Job::Skin`, on the loading thread, returns the parts and the rig.
- **`Assets`:**
  - `give_renderer` checks first whether a model's mesh is skinned (its file read once).
    A skinned model gets `give_skinned`: its skin made per (mesh, split), an instance per
    part (`Skinning::add_instance`) owned by the entity, drawn by its `MeshRenderer` and
    `SkinnedMesh`, and, past the first, by derived parts.
  - Each sync releases the instances of entities that went, poses anew those whose pose
    changed, and lets go the skins of files read again once no model draws them.
  - The skinning instances keep their palette, so a still pose costs nothing on the CPU.
- **The player:** `model_rig(entity)`, and `pose_model(entity, Option<(clip, time)>)` for
  the editor's previews (E12-1b).
- **Measured** (ref-1): CesiumMan's skin and clip ready 36–41 ms after the scene loads; a
  model posed anew every frame makes 0 allocations, 1,030 frames on the Radeon and 1,472
  on the RTX (`zero_alloc_skins`).
- **Tests:**
  - the rig, on CesiumMan, RiggedSimple and Fox: the rest pose's vertices near the mesh;
    the import scale scaling the skinned vertices as it scales the mesh; a clip moving
    them; no clip or a wrong one giving the rest pose;
  - a plain mesh found not skinned;
  - on both GPUs, CesiumMan drawn (5,700 pixels against the scene without him), posed
    halfway through his walk (9,100 pixels changed), drawn the same with a material slot
    (its skin made anew), and gone with his `Model`.
- **Left:**
  - a dropped model still makes an entity per glTF node, joints included (E12-1c,
    question 6; done in E12-1c-1);
  - a file read again leaves its skinned models undrawn until their new skins are made;
  - a mesh drawn by several skinned nodes takes the first's skin;
  - the clips are compressed at every load, not kept in the derived data cache as the
    step's row says;
  - the compression's error and shell distance are taken in the file's units, not the
    model's: a file scaled down on import (the Fox's centimetres) is compressed finer than
    it needs (E12-1b corrects it).

## As built (E12-1b-1, 2026-10-29): a character's clips, and P in place

- **The engine (`engine_app::skins`):**
  - Clips are compressed within ADR 0068's 0.1 mm in the model's units: the error threshold
    and the smallest shell distance are divided by the import scale. The Fox (made in
    centimetres, imported at 0.01) was compressed a hundred times finer than it needed:
    Survey 29,674 → 5,322 B, Walk 4,290 → 2,890 B, Run 19,359 → 11,290 B, as ADR 0068's
    table at its bound.
  - Each `RigClip` gives its largest error in the model's units (`max_error`), its root's
    speed across the ground (`root_speed`), and its loop's seam (`seam`). The seam is how
    far the skin jumps from the last moment back to the first: the end pose is brought back
    so the topmost skin joint stands where the first one's does on the ground, its turn
    about +Y taken out too, then each joint's virtual vertices are compared. The result is
    a length and the joint that jumps most.
  - CesiumMan's walk shows a real seam: its first key is at 1/24 s and its right leg is
    2.6° off at the ends, so it jumps 3.1 cm at `leg_joint_R_1`. The Fox's three clips
    loop within 0.1–0.2 mm.
- **The inspector's Clips** (under the active character's components, once the player made
  its rig; `ClipFacts`, `ClipsView`):
  - a row each: its name ("Clip 2" without one) and length, then its frames, root speed
    ("in place" under 1 cm/s), size and error, then its seam: "loops within 0.2 mm", or
    "jumps 3.1 cm at leg_joint_R_1" with a warning icon from 1 cm;
  - a click on a row shows its first frame, held; ▶ plays the character in place from the
    playhead, ■ stops it there, holding that pose; the loop, the speed (0.25, 0.5, 1 or
    2×), the time ("frame 24 / 48 · 1.00 s") and a bar that scrubs the clip.
- **P** ("Scene: Play Selection in Place"; it was "Sound: Hear Selection in Place") hears
  the selection's sounds and plays its characters where they stand, each its clip (the
  inspected one's chosen clip, the others the clip of that name or their first), until P,
  Escape, another selection or Play. P and Escape put them back in their rest pose. Nothing
  is written into the scene: the editor poses them through `Player::pose_model` each frame
  (`clips_frame`), and a held pose costs nothing (the player poses only what changed).
- **Found and fixed on the way** (the new allocation test showed them):
  - selecting a model allocated a list every frame (the selection's instances for picks);
    it keeps its room now;
  - the outline's mask and depth took the selection's size on screen, so a selection that
    moved, animated or was dragged made new images in the frame graph for each new size.
    Its sizes are whole steps of 64 pixels now.
- **Measured** (ref-1): 0 allocations in 240 editor frames while CesiumMan plays in place,
  on both GPUs (`editor_clips_zero_alloc`). In place, he leaves his rest pose by about
  25,000 pixels and moves about 30,000 between two moments 0.3 s apart; after P he is
  back within 20 pixels (RTX) and 4 (Radeon).
- **Left for E12-1b-2:** the engine's preview of a model picked in the dock, ↑ and ↓
  through the clips, Auto Play, two clips side by side, the floor moving under root motion.

## As built (E12-1b-2, 2026-10-29): the engine's preview of a model picked in the dock

- **The player's live model preview** (`Player::set_model_preview(Option<ModelPreview>)`,
  `model_preview_rig`, `take_model_preview(&mut Vec<u8>)`):
  - a skinned mesh, its material, a turn and a clip's moment (or the rest pose);
  - its skin is made in the background as a model's is (drawn whole), and an instance of it
    is the preview's own (`Assets::set_preview_skin`, posed by the next sync), counted as
    drawn when a file read again retires skins;
  - it is drawn alone as the previews are, with a view and read backs of its own, each
    frame its pose or its turn changed, and nothing otherwise; asking for the same one
    every frame allocates nothing;
  - it stands as its file's scene stands it (the rig's root inverted), turned about its
    middle, framed by the square inside the circle its rest pose's box turns in (with a
    tenth's room), so it fits at every turn about the vertical;
  - its newest pixels are halved into a kept room and copied into the caller's.
- **The editor:** a model file picked in the dock gets Clips under its sections once the
  player made its rig, with the engine's preview at their top (128 px, turned by
  dragging). ▶ plays the chosen clip in the preview, ■ stops it there, the loop, the speed
  and the bar work as for a character, and Auto Play (the dock's) plays its first clip
  when its clips come. `ClipsView` shows a character's clips or a file's (`ClipsOf`).
- **Found and fixed on the way:** frames that alternate between two shapes (the preview's
  pixels coming back two at a time now and then, which leaves a frame without its upload)
  made the frame graph place one of them again into a kept placement with less room, an
  allocation; a placement that grows now gives every one kept the same room. The text
  layout cache's spare layouts have room for as many as it can take out of use.
- **Measured** (ref-1, release): drawing the preview anew each frame costs the GPU 77 µs
  on the RTX (127 µs a frame against 50 µs without it) and 227 µs on the Radeon (469 against
  242), and nothing while it stands still; it is drawn 53–86 ms after it is first asked for
  (CesiumMan). 0 allocations in 240 editor frames while a picked file's clip plays, on both
  GPUs, with and without Tracy; the player's frames with a model and a preview both posed
  anew every frame, 0 in 1,011 (RTX) and 821 (Radeon).
- **Tests:** the preview drawn alone, unchanged when asked the same again, another picture
  posed mid-walk and turned a quarter, nothing once let go (both GPUs); the Fox picked in
  the dock showing Survey, Walk and Run under its drawn preview, ▶ changing the picture
  (about 870 pixels in 0.4 s), ■ holding it (0 pixels).
- **Left:** ↑ and ↓ through the clips, two clips side by side, the floor under root motion
  and the root's path (E12-1b-3); a file whose first part is not the skinned one shows no
  clips; one picture of 128 px, drawn at twice that.

## As built (E12-1c-1, 2026-10-30): a dropped character is one entity

- **The drop's plan** (`engine_editor` `ModelPlan`) for a file one of whose nodes draws its
  mesh skinned (`engine_assets` `OutlineNode::skin`, from the JSON alone):
  - the first such node (in the scene's order) is the root, named after the file, with
    that mesh's model and slots, turned and scaled as the file places that node (its
    place is where it is dropped, as for a file of one node);
  - every other node that draws a mesh is an entity under the nearest entity above it (the
    root when none is), at its place against that one in the file: a mesh under a joint
    sits where the skeleton's rest pose holds it, until attachments make it follow
    (E12-1c-3);
  - the joints and every node that draws nothing are not entities;
  - files that draw nothing skinned keep an entity per node, unchanged.
- **Resting on the floor:** the outline's boxes are the rest pose's. CesiumMan, the Fox
  and RiggedSimple's rest poses fill their positions' boxes to the micrometre, now a
  check of the rig test, so the drop stands a character's feet on the surface.
- **Tests:**
  - plans of a CesiumMan-like file (a turned node above the skeleton: one entity, turned
    from z up to y up, its box turned with it), of a Fox-like one (the skeleton under a
    node of its own beside the mesh, imported at a hundredth: one entity), and of a file
    whose skin no node draws (an entity per node);
  - a knight whose sword sits in the hand's joint, a gem on the sword and a hat on the
    skinned body: four entities, each at its place in the file at two import scales;
  - the real files (each one entity drawing its skinned mesh);
  - on both GPUs, CesiumMan dragged from the dock onto a floor: one new entity without
    children, upright, his feet on the floor's top (within 0.06 mm), drawn skinned with
    his Clips once selected.
- **Left:** scenes saved with a character's joints as entities keep them; a skin drawn by
  a node other than the first skinned one keeps its own place under the root (two
  characters in one file are two entities, the second under the first).

## As built (E12-1c-2a, 2026-10-30): a character's joints in the viewport

- **The player:** `Player::model_joints(e, &mut Vec<Affine3A>)` gives a skinned model's
  joints where its pose puts them: in the entity's space (the entity's transform places
  them as it places the skinned mesh), in the skeleton's order, into the caller's room.
  `Player::skinned_models()` lists the entities drawn skinned. The rig's `Posing::joints`
  samples the pose as the palette does: its joints skin as the palette does, now a check
  of the rig test.
- **The editor:** the selected characters' skeletons are drawn as sticks from each joint
  to its parent, white and in front of everything (Blender's Stick, In Front), made each
  frame from the pose the player draws. **View: Skeletons** (`view.skeletons`) adds every
  other character's, in grey. No allocation while a selected character plays in place,
  on both GPUs and with Tracy.
- **Tests:** CesiumMan selected shows 20 sticks of his 21 joints, within his height, that
  follow his walk (joints 0.37–0.44 m apart between two moments 0.3 s apart) and go with
  the selection; View: Skeletons switches.
- **Left:** Tab's joint mode (a click picks a joint) and the inspector's joint tree
  (E12-1c-2b); the sticks are one pixel wide.

## Consequences

- New source files: `.agraph` and `.anim`, and a section in models' `.meta`; a new component
  in scenes, saves and the network schema.
- The player evaluates characters on the job system every tick and frame; a scene with no
  `Animator` pays nothing.
- Scripts gain calls on the component, the team's to approve.
- Inertialization needs a proper patent search before it ships (the research found none).

## Questions for the team

The two notes' questions, merged: the UX note's 1 to 5 are 1, 6, 7, 8 and 9 here; the
runtime note's 1 to 6 are 2, 3, 4, 5, 1 and 7. Each note gives the options' costs in full.

1. **What makes a character animate.** (a) An `Animator` naming a graph file; (b) clips played
   from code only; (c) both: the component plays a clip or a cross-fade, or runs a graph.
   Recommendation: c, with the calls above.
2. **When the animation decides.** (a) In ticks, poses drawn every frame between the last two
   (the same at any frame rate, saved and replicated); (b) all of it every frame.
   Recommendation: a.
3. **How a change of state blends.** (a) Cross-fade; (b) inertialization; (c) either, per
   transition, inertialization when interrupted. Recommendation: c.
4. **Keeping walk and run in step.** (a) By fraction of the cycle now; (b) markers as well.
   Recommendation: a now, b with the timeline.
5. **What a clip may animate besides joints.** (a) Joints and events only; (b) also number
   fields of components; (c) b plus on/off and asset switches. Recommendation: b.
6. **Whether a skeleton's joints are entities.** (a) No: one entity, joints in its tree;
   (b) yes, as Unity; (c) a, listed under the model in the tree. Recommendation: a.
7. **Where animation data lives, and the files' names.** (a) Imported clips' settings and
   events in the model's `.meta`, editor clips in `.anim`, graphs in `.agraph` (beside
   `.mgraph`; `.animgraph` and `.anim` for graphs were the other names weighed); (b) every
   clip extracted to `.anim` at import; (c) imported clips read-only until extracted.
   Recommendation: a.
8. **How keys are made.** (a) No record mode: K keys what changed, ◆ one field, an animated
   field keys itself; (b) a record switch; (c) only K and ◆. Recommendation: a.
9. **The last seconds of play, kept.** (a) Live only; (b) the last 10 s of each character's
   states and inputs while the editor plays, scrubbed when paused; (c) b with the poses.
   Recommendation: b, in E12-6.
