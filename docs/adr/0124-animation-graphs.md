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

## As built (E12-1c-2b, 2026-10-30): Tab's joint mode and the Skeleton section

- **Tab over the viewport on a character** (`anim.edit_joints`, Animation: Edit Joints;
  on brushes Tab still edits faces): the character's joints take the pointer, as faces do.
  - A small square is drawn at each joint.
  - The joint nearest the pointer within 8 pixels lights up light blue, with its name.
  - A press on it picks it: orange, its name in the status bar, the selection unchanged.
  - Tab, Escape, or a press in the view on no joint ends the mode (that click selects). A
    press on the panels keeps it, so the Skeleton's rows pick too.
  - The gizmo and the handles hide meanwhile.
- **The inspector's Skeleton**, after the Clips: the active character's joints as a tree,
  each indented under its parent. The picked one is lit; a click on a row picks its joint.
  The skeleton's own nodes above the skin's joints (CesiumMan's `Z_UP` and `Armature`)
  are listed too.
- **Tests:**
  - on both GPUs, CesiumMan selected: Tab enters his joints; a click on a joint alone in
    the view picks it, keeping him selected; a click on another joint's row picks that one
    without leaving the mode; Escape leaves it;
  - a skeleton's depths, parents first;
  - 0 allocations while he plays in place with his Skeleton shown, both GPUs, Tracy.
- **Left:**
  - the gizmo does not turn a picked joint yet (a pose needs a place to be kept: the
    Animation tab's keys, E12-3 and E12-4);
  - F frames a character by its origin, not its skinned box;
  - the tree has no filter yet.

## As built (E12-2a, 2026-10-30): the `Animator` plays a clip

- **`engine_runtime::Animator`** (reflected, saved):
  - `clip`: a name, with the new `Hint::Clip` so the editor lists the model's clips. An
    unnamed clip goes by its shown name `Clip N`; an unknown name plays the first.
  - `speed` and `paused`.
  - `time`: seconds into the clip, saved, so a load goes on from there.
  - Not saved: `length` (the clip's, written back by the player for the ticks to wrap
    at), `previous` (the time before the last tick, shifted alike across a wrap) and
    `ticked`.
- **Ticks** (`Animators::advance`, after the scripts' `on_tick` and before `Game::tick`):
  `time += dt × speed`, wrapped into the clip once its length is known.
- **Frames** (`Assets::animate`, before the palettes are set): the clip at
  `previous + (time − previous) × alpha`, wrapped. While the world stands still the frame
  poses at `time` itself: alpha moves on without ticks, and no pose should.
- **The editor's holds win:** a skinned instance keeps whether the editor holds its pose
  (`Player::pose_model(e, Some(..))`). `None` lets it go to the animator's pose, or to the
  rest pose without one. P in place and the Clips' playhead hold; stopping lets go.
- **The editor:** the `clip` field is a choice of the active character's clips; a pick is
  one undo step.
- **Tests:**
  - an animator's ticks and frames: the wraps forward and backward, the seam, paused, an
    unknown length;
  - several animators moving together;
  - the clip found by name (`Clip 2` for an unnamed one);
  - on both GPUs, CesiumMan with an `Animator` (the scene saved with it):
    - half a second of clip in 30 frames;
    - the frame's pose is the clip at the frame's moment between the two ticks, to
      0.01 mm;
    - a held pose stays while the clip goes on, and letting go returns to the animator;
    - a still world and a paused animator hold;
  - the editor's clip field: a pick writes the name, a step each;
  - 0 allocations while a second character plays through its animator beside the posed
    one (both GPUs).
- **Left:**
  - the `Animator`'s own script calls (`play` with a fade, `set`, `trigger`), cross-fades,
    layers, root motion, graphs (the rest of E12-2, E12-5);
  - a joined network client's animators do not move yet (its ticks are its prediction's);
  - scenes made before keep no animator: characters stand in their rest pose until one is
    added.

## As built (E12-1c-3a, 2026-10-30): sockets follow their joints

- **`engine_runtime::JointAttachment { joint }`** (reflected, saved; the found joint index
  and rig are not): an entity right under a skinned model is a socket on that joint, as
  Godot's `BoneAttachment3D`. Its children are the attachments, in the joint's space. This
  is the ADR's "child entity naming a joint", made a socket so an attachment keeps a
  transform of its own.
- **Placed in the tick, not the frame** (`Assets::place_sockets`, after the animators move
  and before `Game::tick`; in a world standing still, at the tick's early return): the
  socket's `Transform` becomes the joint in the character's pose then (the editor's held
  pose, else the animator's at its time, else rest). Transforms then propagate as for any
  entity, so the tick's gameplay sees the sword where the hand is. The frames between two
  ticks draw it as every transform is drawn, between the two tick poses: at 64 Hz that
  stays within the skinned hand, with no second, frame-time path into the extraction.
- **Cost:** sockets are grouped by character; each character with sockets has its
  skeleton's joints made once per tick; a socket already in place is not written, so the
  propagation's incremental pass skips still characters. The player's `assets` is shared
  by the tick and the frame's closures through a `RefCell`.
- **Tests:**
  - on both GPUs, CesiumMan with an animator, a socket on his right hand and a cube under
    it:
    - held halfway, the socket is on the joint (0 m off) and the cube in his hand (within
      0.1 mm);
    - let go, the socket goes with the walk (0.52 m in 30 frames);
    - standing still, it stands;
    - a socket on a joint he lacks stays put;
  - 0 allocations with the socket and its cube placed every tick (both GPUs, Tracy).
- **Left:** attaching by a drop onto a joint (the viewport's joint mode, a Skeleton row)
  and a file's meshes under joints dropped as sockets (E12-1c-3b); a socket deeper than
  right under its character.

## As built (E12-1c-3b, 2026-10-30): attaching in the editor

- **A row dropped on a joint** (`RowPlace::Joint`): with a character's joints showing
  (Tab), dragging a tree row carries that row alone. The character stays selected, so its
  joints stay in view and in the inspector's Skeleton.
  - The drop targets are a joint's square in the view, or its row in the Skeleton (the
    shell reports the row under the pointer).
  - On release the rows go into the joint's socket, made where the character has none: a
    `JointAttachment` named after the joint, right under the character, starting where the
    joint is in the pose last drawn.
  - Each keeps its place in the world (Alt its local placement), in one undo step;
    `move_rows`' move is shared as `place_rows`.
  - Neither the character nor what carries it can go onto its own joint.
- **A dropped file's meshes under joints come in sockets:** `GltfOutline::skins` lists
  each skin's joints. In a skinned file's plan, a mesh with a joint between it and its
  nearest entity hangs under a socket on the joint nearest it. One socket per joint,
  right under the root, where the rest pose holds the joint; the mesh is at its place
  against the joint.
- **Tests:**
  - a sword's row dragged onto the hand's Skeleton row: the man stays selected, the hint
    reads "Sword onto Man's hand", one step makes the socket at the joint, the sword
    keeps its place in the world, a shield onto the same joint shares the socket, and
    undoing takes the socket away with the sword's move;
  - the knight's plan with a socket on the hand (two import scales);
  - a dropped plan's socket made with its `JointAttachment` and the sword under it.
- **Left:** a socket deeper than right under its character; the drag's highlight of the
  joint under the pointer is the joint mode's own (no tree line).

## As built (E12-2b, 2026-10-30): cross-fades and `animator:play`

- **`Animator::play(clip, fade)`:** starts `clip` from 0 and fades from what played over
  `fade` seconds; a fade of 0 or less switches at once.
  - The clip that played becomes `from_clip`, at its `time` (`from_time`).
  - A `play` during a fade drops the clip that was fading out, and the clip that was
    fading in fades out from where it is. A cross-fade, not inertialization: question 3c
    leaves that per transition, for the graphs.
- **State:** `from_clip`, `from_time`, `fade` and `faded` are hidden (saved, not shown),
  so a load goes on fading. `from_length`, `from_previous` and `faded_previous` are not
  saved; they mirror `length` and `previous`.
- **Ticks:** both clips move on by `dt × speed`, each wrapped at its own length; `faded`
  grows by `dt` (held while paused). The fade ends the tick after `faded` reaches `fade`,
  so the last frames between ticks still blend toward weight 1.
- **Frames:** the new clip's weight is `faded / fade`, between the last two ticks as the
  times are. `Posing` samples both clips and blends them with two normal layers
  (`engine_anim::Blender`, its weights 1 − w and w) into a third pose kept with the
  others: sized once for the largest rig, so blending allocates nothing. A weight of 1
  skips the second sample. Sockets take the same blended pose in the tick.
- **Scripts:** `Animator` views have `play(clip: string, fade: number?)`. It is matched
  through the same `__namecall` atoms as `Transform`'s methods, and the `.d.luau` types
  declare it. A clip that is not text, or a fade that is not a number, is the script's
  error.
- **Tests:**
  - ticks of a fade: both clips move on, the weight between ticks, the end the tick after,
    a fade cut short by another, a switch at once;
  - a fade's blend on every test rig, where the skin is: weight 0 is the clip fading out,
    weight 1 the other (exactly), and halfway is neither;
  - a script's `play` with and without a fade, and the error for a name that is not text;
  - the types: `play` declared, the hidden fields left out;
  - on both GPUs, the Fox fading from Survey into Run over half a second. Midway, the
    drawn pose is neither clip and nearer each than they are to each other. A save then
    keeps the fade, and once the fade is over Run alone is drawn;
  - 0 allocations with a fade going on all along and started again every 200 frames (both
    GPUs, and the tracy build).
- **Left:** layers, root motion, the animator's other script calls (`set`, `trigger`,
  `state`, `on_animation_event`), sync by the cycle's fraction (question 4a, with the
  graphs), and a joined client's animators.

## As built (E12-1b-3a, 2026-10-30): the preview's floor, In Place, the arrows

- **The model preview's floor** (`ModelPreview::floor`, ADR 0124's E12-1b-3):
  - The floor is the blockout's grid on a square of 8 × 8 cells, six times the framing's
    reach. It is made once when first asked for (`Previews::make_floor`) and drawn as a
    second item under the model.
  - It sits at the posed box's lowest point and is centred on the model's origin, not on
    the posed box, which sways with its limbs. It turns with the model.
  - Its offset is wrapped by whole cells along its own axes, so a walk of any length never
    leaves the square.
  - With a floor, the preview's pose takes its clip's root motion out (`Look::in_place`).
    The model stays on its spot and the floor goes by under it.
  - The skinned instance's bounds follow its pose (`Skinning::update_bounds`), and the
    preview frames them. Without a floor, a model carried along by its clip looks the same
    in the preview; with one, the floor shows the motion.
- **`Rig::scene_motion(clip, from, to)`:** a clip's root motion over a span of play in its
  file's scene (+Y up, its units), as the preview stands it. The editor adds the step of
  each frame it plays.
- **The editor:**
  - a picked model file's preview always has its floor;
  - each frame its clip plays, the floor goes back by the step and turns back by its turn
    (`floor_walked`);
  - **In Place** (beside the speed) holds the floor still;
  - choosing another clip puts the floor back.
- **↓ and ↑ through the clips:** a press in the Clips gives them the arrows (`CLIPS_KEYS`,
  a hover region as the tree and the dock have). "Clips: Next Clip" and "Previous Clip"
  choose as a click on the row does, held at the ends. With Auto Play on, a picked file's
  clip plays at once.
- **Tests:**
  - on both GPUs, a walker generated in the test:
    - its floor covers 7,936 of 16,384 pixels (3,706 without);
    - moving the floor 0.3 changes 271 pixels;
    - half a second into `Turn`, with a floor it differs from its start by 0 pixels (its
      turn taken out), and by 768 without one;
    - `scene_motion` gives 0.5 along +z for half a second of `Walk` and a quarter turn's
      half for `Turn`;
  - the Fox picked in the dock:
    - playing changes 2,045 pixels of the inspector with its floor, against about 870
      before;
    - a click on Walk's row, then ↓, ↑ and ↑, chooses Run, Walk and Survey through the
      real key path;
  - the floor's walk math, and the arrows held at the ends;
  - 0 allocations with the floor drawn and moving every frame (the player's test) and
    with a picked file's clip playing (the editor's), both builds.
- **Left (E12-1b-3b):** two clips side by side (Ctrl+click), and the root's path and
  facing drawn on the floor.

## As built (E12-5d-1, 2026-10-30): the graph's canvas in the editor

- **One tab kind for both graphs:** `engine_editor::graph::GraphDoc` holds an `.agraph` too (`anim`: `engine_editor::agraph::AnimDoc`, the material `graph` empty), so its tab, Ctrl+S and Ctrl+W, Ctrl+Tab, the one history's file steps (`History::change_file`), its autosave (`.ofelia/autosave/<name>-<hash>.agraph`) and the session are the material graph's; a step of a closed graph undone opens it again. The canvas's view, pointer, back and grid, box selection and boxes' look are the material graph's (`graph::draw_back`, `draw_select_box`).
- **What it shows:** the first layer's machine (`layers_in_order`, the first root that is one). A state is a box with its name and what its pose plays (`Walk`, `Blend1d speed: Walk · Run`, `Machine`, at most 40 characters); the entry's title bar amber, an Entry tag and arrow into it; an Any State box at its machine node's own `at` (a machine shows as a node on no canvas, so its place is free for this, as Unity keeps its Any State's position on the machine); transitions as arrows edge to edge, those between one pair both ways side by side (half a 12-unit lane to each one's right), several one way in lanes about the middle, one into its own state a loop over its box.
- **The pointer:** a press on a state selects and moves it (whole units at the end, Escape takes it back); on its edge (6 pixels, at most a quarter of its height, half of it outside) or with Ctrl, a transition is drawn to the state it is dropped on (the file's defaults, selected so its condition is typed next; into itself refused), or on the empty canvas makes a state there with the transition into it; a click on an arrow selects it; a box selects the states it meets.
- **Edits** (each a step): states added by the search (a clip, a 1D or a 2D blend, the graph's first number parameters the blend's), named `New State` (numbered in its machine), the first of a machine its entry, a layer and a machine made where the graph shows none; deleted with every transition into or out of them and their poses where nothing else uses them (a machine's states with it), the entry moved to the first state left; the entry set. New parts' ids are four random hex digits no part of the file has (eight after 32 taken tries), so two branches' new parts merge apart.
- **The inspector** (`AnimRow`): a state's name (unique in its machine), entry, pose kind, clip or blend parameters and clips, speed; a transition's condition, compiled as typed against the parameters in their names' order (`agraph::compile`, as the cook does) and its error said with its character (``no parameter `speeed` (at 1)``), blend, priority (whole), exit time, offset, keep phase, inertialize; with nothing selected, a section per parameter (name, kind, starting value made one its kind holds; × deletes it) and Add Parameter. A parameter renamed is renamed in every condition (its words, as the conditions read them), blend and layer.
- **Problems:** `AnimGraphFile::problems` and the conditions that do not compile, found when the file changes, on the state (a pose's on each state showing it) or the transition they are about, else on the graph: framed in red with why, the first in the bar, the selected one's under its fields.
- **Measured:** no allocation in 240 frames of an animation graph's tab (the pointer over states, edges, arrows and their labels), as the material graph's.
- **Tests:** the file opened and read back; boxes, the entry, Any State, arrows edge to edge and side by side, loops and hits; the pointer's moves, links (into itself refused, onto the empty canvas a state), arrow clicks and box selection; deletes with their transitions and poses, the entry moved; states added (a blend's parameters, names numbered, a graph without a machine given one, New Animation Graph's Idle and its problem); a state's and a transition's rows, conditions checked with their places, parameters added, renamed (conditions followed), retyped, deleted; every edit undone and its text read back; in the editor, the tab opened, a transition drawn by the mouse, its condition typed and checked, Delete, undo and redo, the parameters, Ctrl+S, Ctrl+W and the undo opening it again, New Animation Graph and Space's search; on the GPU, the canvas drawn (the entry's amber, the states' blue, Any State, the Entry tag, the broken arrow red) and gone behind the scene's tab.
- **Next (E12-5d-2):** layers and masks, blends drawn as points with Place by Speed, machines in machines (Tab), the character previewed in the corner; then the live state in Play (E12-5e).
- **Settled by the team (2026-10-30):** Any State's place stays in its machine node's `at`
  (5a); New Animation Graph is to give the new graph to the selected characters' `Animator`
  (4a), as New Material gives its material to the selected models; transitions drawn in the
  editor are to inertialize by default, the file's default staying a cross-fade (2a). The
  last two come with E12-5d-2.

## As built (E12-5c, scripts, 2026-10-30): the scripts' calls on a graph

- **`animator:set(name, value)` and `animator:trigger(name)`** on `Animator` views (`engine_app::script_views`), matched through the same `__namecall` atoms as `play`.
  - The name is read in place from the stack. Its slot is found in the world's `GraphParams` for the animator's `graph`: one hash lookup and a scan of at most 16 names.
  - `Animator::set_slot` / `fire_slot` write it and mark it in `params_set`; a slot past the 16 does nothing.
  - `set` takes a number, or a boolean (1 or 0).
- **Errors** are the script's at its line:
  - an animator playing no graph;
  - a name its graph lacks, with the names it has of the call's kind (``Animator's graph has no parameter `sped` (it has: armed, speed)``);
  - a trigger set, or a number or flag fired, naming the right call;
  - a value or name of the wrong type.
- **A graph the player has not read yet** (`GraphParams::knows`) drops the call. The player reads every graph an animator names at each tick's start (E12-5c-1), so only a graph given in the same tick is not read yet.
- **`animator.state`** reads the hidden field through the views' text path: kept strings, nothing made while it stays. Writing it is refused (``Animator.state is read-only …``).
- **Types** (`engine_app::script_types`):
  - `GraphNames` holds the project's graphs' parameter names, sorted and each once. `add_text` reads a file's header and `params[…]` lines only.
  - `definitions_with(&names)` declares `export type AnimatorParam` (numbers, flags and whole numbers) and `AnimatorTrigger` (triggers) as unions of string choices, `string` without names.
  - On `Animator` it declares `set(self, name: AnimatorParam, value: number | boolean)`, `trigger(self, name: AnimatorTrigger)` and `state: string`.
- **The editor makes the types again when the project's graphs change:**
  - `AssetDatabase::revision()` counts scans that found a file new, written anew, moved or gone.
  - A frame compares it; a new revision hashes the graph entries; a new hash reads the graph files.
  - The new `types/ofelia.d.luau` is written and given to the checker.
- **Completion inside the quotes:**
  - Luau offers string choices only inside a closed string, and the editor closes no quotes. So the completion's question closes, at the caret, a string the caret's line leaves open (`complete::open_quote`).
  - `set("s` offers `speed`; before, Luau answered as at an expression with every global.
- **The checker** (Luau's old solver) underlines a misspelt or wrong-kind name at its string in `--!strict` scripts, as New Script writes them: `Expected this to be '"armed" | "speed"', but got '"sped"'`. Nonstrict scripts (no mode line, Luau's default) report nothing for it.
- **Measured** (`scripts::tests::measure_animator_graph_calls --ignored`, release, ref-1; 10,000 calls a tick minus an empty loop; medians of 51 ticks, four runs):
  - `set` 69–75 ns, `trigger` 66–71 ns, `state` 51–58 ns;
  - 0 allocations in 600 ticks with ten riders setting, firing and reading every tick.
- **Tests:**
  - the binding's slots, errors, state and missing graph;
  - the riders in `zero_alloc_scripts`;
  - the types' unions;
  - the analysis in strict and nonstrict, and completion in closed and typed-open strings;
  - the editor: completion as typed, a misspelt name underlined, and the types following a graph written anew;
  - the player: a script taking the Fox's graph Idle → Move → Alert → Idle by name.
- **Left:** `animator:in_window(name)`, with clip windows.

## As built (E12-5c-1, 2026-10-30): the player plays graphs

- **The `Animator`** names its graph in `graph` (an `.agraph`; none plays `clip` as before).
  Its parameters are `params`, by slot (a slot is a parameter's place among the graph's
  names in their order); `params_set` marks the slots set by `set`/`trigger` (or
  `set_slot`/`fire_slot`); the others follow the graph's values, its starting ones until a
  transition changes them, so a graph edited later moves its defaults with it. `state` is
  the first layer's state by name.
- **A graph file** (`engine_app::graphs`) is read the first time an animator names it (or
  taken from the editor while it edits it, `Assets::set_anim_graph`, as clips are) and
  turned into a `GraphDesc`: ids become indices (layers by `order`, the rest by id); a
  machine without an entry starts in its first state, a state without a name is named by
  its id, an id that names nothing becomes an index out of range for the cook to name.
- **Its parameters reach scripts before they run:** at each tick's start the player passes
  over the animators and reads each graph not read yet, telling its names to the world's
  `GraphParams`; a run of animators naming one graph looks it up once. So a script's
  `set` finds its slot even on the tick its character appeared, before its model loads.
- **Cooked once per rig** (file × rig, the rig kept alive by the cache): the clips found by
  name among the model's (`Clip 2` for an unnamed second, as `Animator::clip`), each one's
  length and whether its `.meta` plays it once; the additive layers' references prepared.
  What cannot be cooked is told once in the log. A model read again or its clip settings
  edited cook its graphs again; a graph file written anew is read and cooked again.
- **Each character** gets a `GraphState` when it plays a graph: its entry states, the
  parameters its animator set put in, weighed once (a tick of 0 s), so a still world (the
  editor) shows its first pose. A cooked graph or file that changed makes a new state; a
  generation number makes the check one comparison a character.
- **A tick** (after the scripts' and the animators', before the game's): the set parameters
  go in, the graph ticks by `dt` times the animator's `speed` (held while `paused`, or at a
  speed below 0: graphs do not play backwards), every parameter comes back (a trigger a
  transition took is 0 again), and the first layer's state is written to `state` (its room
  kept). Ticks the network plays again move nothing: a graph's state is the player's, not
  part of what the network rolls back.
- **A frame** poses each character from its graph between its last two ticks
  (`Posing::palette_graph`, one `GraphWork` sized for the largest rig); sockets and
  `Player::model_joints` take the same pose (sockets at the tick's moment).
- **A graph that cannot be read** shows the rest pose (and is told once in the log).
- **Not yet:** a graph's root motion stays in its pose and its clips fire no events (5c-2,
  with saves of its state); its clips are the model's, not the animator's own `.anim`
  clips; inertialized transitions cross-fade (5b-2, in parallel).
- **Measured:** 0 allocations in 607 frames of CesiumMan's `zero_alloc_skins` with a fourth
  character playing a graph (two states cross-faded by a number set every 120 frames, a
  trigger from any state every 90, a layer masked to an arm, an additive one), next to the
  others; the Fox GPU test on the Radeon goes Idle → Move → Alert → Move → Idle by its
  parameters, the editor's copy renaming a state, a missing graph at the rest pose.

## As built (E12-5b-2, 2026-10-30): inertialized transitions and blends cut short

- **Bollo's inertialization, chosen by evidence** over Holden's dead blending (the research
  note's new section): the destination plays at full strength from its first frame, there
  is nothing to tune, and a frame adds a few multiply-adds per joint (no sine or cosine).
  Dead blending's case, a long blend between very different poses, keeps its cross-fade.
  Patents checked 2026-10-30 (Google Patents, Justia): none found for either.
- **`TransitionDesc::inertialize`** (the `.agraph` file's `inertialize`, mapped by
  `engine_app::graphs`): only the destination plays.
  - Each joint's offset of the source from it (the rotation `Δ = source · destination⁻¹`,
    the translation's and scale's differences) decays over `blend`, with the difference of
    their velocities.
  - The decay is `x0·P(u) + v0·T·Q(u) + a0·T²·R(u)` in closed form. Bollo's initial
    acceleration is kept at 0 or more, and his time is shortened where a joint would pass
    its destination, so none overshoots.
  - The whole velocity is kept (Bollo projects it on the offset). A rotation decays along
    the chord to no turn, normalized.
  - The pose is exactly the destination after `blend`.
- **Blends cut short:** a transition taken while its machine still blends (a cross-fade or
  an inertialization), or one into the pose it leaves (which cannot fade into itself),
  goes on as an inertialization from the pose shown then. The decay playing is carried
  into its source, and the state fading out is sampled no more. One set of decays per
  layer: no frozen pose, no stack of sources.
- **`AnimGraph::capture(&mut state, clip, rest, &mut work)`**, after each tick, before the
  next and before `pose`:
  - It takes the layer as the frames showed it at the transition and a quarter tick
    before. That gives the source's velocity: a decay playing then bends within a tick, and
    a quarter tick measured 0.2 % off where a whole tick was 7 % off.
  - It compares that with the destination at the transition and a tick after, and begins
    each joint's decay. It samples nothing unless the tick began one
    (`GraphState::wants_capture`).
  - It is taken after the tick, not at the first frame: at 30 fps two ticks pass between
    frames, so a frame's capture would have to keep each tick's source (a queue) or pop.
    The pose stays a function of the ticks' state and `alpha`, the same at any frame rate,
    and `pose` keeps `&GraphState` with its arguments unchanged.
  - A transition never captured plays without its offsets, so characters nobody sees can
    skip it. The player captures after each animator's tick (`Assets::tick_graphs`,
    `Posing::capture_graph`, in the frames' graph work).
- **Room only where a graph may inertialize** (an inertialized transition, a fade into a
  state that can be left, a pose entered again; `GraphState::size_bytes`):
  - per joint of each such layer 112 bytes (offsets, velocities, times, brakes), plus the
    phases and weights a tick earlier;
  - a tick touches it only when it takes a transition and on the tick after; a frame only
    while a decay plays (otherwise a clock compare);
  - the benchmark's 52-joint character: 7,056 bytes, against 576 without.
- **The first tick** takes an inertialized transition at once (nothing was shown before
  it). The tick after a cut (`blend` 0) does not read the source's velocity across it.
- **Measured** (`graph_bench`, ref-1, release, now under the engine's allocator as the
  player is; the best of 8 runs alternating with main's):
  - ticks unchanged: 0.09–0.12 µs a character;
  - a frame on one thread at 100 / 1,000 / 10,000 characters: 4.32 / 4.33 / 4.60 µs a
    character inertialized, against main's cross-fades at 4.51 / 4.68 / 4.73 (fades cut
    short, now inertialized, 4.40 / 4.50 / 4.72);
  - 16 workers: 0.30 / 0.79 / 5.75 ms against 0.31 / 0.89 / 6.1;
  - a capture 8.1–8.4 µs once per inertialized transition (9.5–10 µs cutting a fade
    short).
- **Tests:**
  - an inertialized transition's first pose is its source's (3·10⁻⁸ rad) at its speed
    (1.3 %), only the destination is sampled while it blends, and it ends exactly at the
    destination (10⁻⁵);
  - a fade cut short halfway goes on from the pose shown (bound 10⁻⁴ rad, 0 measured) at
    its speed (0.3 %), the state fading out never sampled again;
  - an inertialization cut short does likewise (0.2 %), its decay carried in;
  - a transition into the pose it leaves starts the clip again without a pop;
  - no room for a graph of cuts only; the first tick's inertialized transition is taken at
    once; a transition never captured plays without its offsets;
  - no allocation in 800 ticks, captures and frames of 32 characters with 896
    inertializations captured (`tests/graph_zero_alloc.rs`), nor in the player's frames
    with the dancer's jump inertialized (`zero_alloc_skins`); the fox's inertialized `look`
    transition plays in `a_fox_plays_an_animation_graph`.
- **Left (team decision, 2026-10-30, 3б):** the decays are kept for every joint (about 7 KB
  for a 52-joint character); keeping translations and scales only where a clip moves them
  (about 2.5–3 KB) is postponed until crowds need it.

## As built (E12-5b, 2026-10-30): the graph's runtime in `engine_anim`

- **What a graph is made from:** `engine_anim::GraphDesc`, its parts by index (parameters,
  layers, nodes, states, transitions), names for clips, parameters and joints. `engine_anim`
  knows nothing of files: the player turns an `.agraph` into one (5c).
- **Cooked per rig:** `AnimGraph::cook(desc, clip, skeleton)` binds clip names (a
  `ClipInfo`: its index among the character's clips, its length, whether it plays once) and
  mask joints, compiles the conditions, and lays everything out flat (`u16` indices, boxed
  slices).
  - Transitions are grouped by source, each machine's "from any state" group first, every
    group in priority order and then as written.
  - Blends' clips become leaves: a `Blend1d`'s sorted along its parameter, a `Blend2d`'s
    triangulated at cook (our own Bowyer–Watson).
  - Masks are filled per joint down each named joint's subtree, deeper names over their
    ancestors'.
  - What cannot be cooked is left out and named (`GraphProblem`): a clip or joint the rig
    lacks, a condition that does not compile, a 17th parameter, a reference out of range.
    The rest still plays: a state whose clip is missing shows the rest pose.
- **A character's state:** `GraphState::new(graph)` allocates once: 16 parameter values,
  each machine's state and fade, each clip's and blend's phase now and at the tick before,
  each layer's leaves' weights now and before. `set`, `get`, `fire` (a trigger counts up
  until a transition takes it).
- **A tick** (`AnimGraph::tick`), the same at any frame rate:
  - each machine takes up to four transitions in a row, at the tick's start: from any state
    first (never into the state it is in), then its state's, the first whose exit time has
    passed (in the loop for one that loops) and whose condition holds; its triggers are used
    up; the destination starts at its offset or the fraction the source played; a machine
    shown by a state starts at its entry;
  - its fade then moves on by the tick (a fade begun now is `dt` in at its end);
  - every clip is weighed top down (layers, machines' fades, blends' weights), and the
    weighed clips' and blends' phases move on. A blend moves at its clips' lengths weighed
    together, so its clips stay in step.
  - Phases are unwrapped fractions (past 1 on later loops, whole loops dropped from both
    readings past 1,024), so a frame between two ticks blends across a loop's seam.
- **A frame** (`AnimGraph::pose`): each layer's leaves sampled at their phases and weights
  between the last two ticks (`alpha`), the heaviest 16 when more, blended by one
  `Blender::blend`; then stacked over the rest pose: a normal layer moves what is below
  toward its pose by its weight times each joint's mask weight; an additive one adds its
  difference from its clips' first frames.
  - `AnimGraph::prepare` samples those first frames once per rig; without it they are
    sampled every frame (15 % of the benchmark's frame).
  - `GraphWork::new(joints)` holds the poses a frame works in; nothing is allocated.
- **A transition taken while its machine still fades** drops the state fading out (the
  inertialized hand-over is a later step), as do `inertialize` transitions for now: they
  cross-fade.
- **Measured** (`cargo run -p engine_anim --release --example graph_bench`, ref-1): a
  52-joint humanoid of synthetic clips (every joint turning in each), idle and a 3×3 blend
  of nine clips over two parameters cross-faded both ways, an upper-body layer masked to the
  spine, an additive lean; wishes changing every quarter second so a quarter of the
  characters is in a fade:
  - a tick: 0.10–0.13 µs a character;
  - a frame: 4.4–4.5 µs a character on one thread (5.2–5.3 without `prepare`);
  - 10,000 characters' frames on 16 workers: 5.7 ms (100: 0.30 ms, 1,000: 0.79 ms).
- **Tests:** states by their parameters (a fade in, a blend halved, a trigger taken and
  used up, an exit time's moment within two ticks); 1D blends between their two points and
  clamped outside, 2D blends by barycentric weights inside and the nearest edge outside;
  masks (the arm alone, then the root alone); the cook's problems named with the rest still
  playing; a frame between ticks between their weights; an additive layer adding a turning
  clip's difference (and nothing for a still one), prepared first frames posing alike;
  Delaunay; and no allocation in 800 ticks and frames of 32 characters with fades, triggers,
  a 2D blend, a masked and an additive layer (`tests/graph_zero_alloc.rs`).
- **The scripts' calls** were settled by the team (2026-10-30): (a)
  `animator:set(name, value)`, `animator:trigger(name)`, `animator.state`,
  `animator:in_window(name)`, **with completion**: the project's graphs' parameter names go
  into the generated Luau types as string choices, so the editor and VS Code offer
  `"speed"` in `set` and the triggers in `trigger`, and underline a misspelt one.

## As built (E12-5a, 2026-10-30): the `.agraph` file and transition conditions

E12-5 is taken in five steps: **5a** the file and the conditions (this one); **5b** the
graph's runtime in `engine_anim` (cooked flat arrays, the tick's decisions and the frame's
pose stack, measured on 100, 1,000 and 10,000 characters); **5c** the `Animator` playing a
graph in the player (parameters, saves, events, root motion, the scripts' calls); **5d** the
graph's canvas in the editor (states, transitions, blends, layers and masks); **5e** the live
state in Play.

- **The file** (`engine_scene::AnimGraphFile`, `.agraph`, `AssetKind::AnimGraph` "animation
  graph", its id inside), in the editor's grammar as the research note sketched:
  - **`params`** by name (scripts and conditions name them): a kind (`Float`, `Bool`, `Int`,
    `Trigger`) and the value it starts with.
  - **`layers`** by id: a name, an `order` (the smallest plays first, under the others), the
    `root` node it plays, `Normal` or `Additive`, a constant `weight` or a parameter's
    (`weight_param`), a `mask` of joints each with a weight for its subtree (empty: all).
  - **`nodes`** by id: a `Clip` (by its name: the model's, or one of the animator's own),
    a `Blend1d` along a parameter or a `Blend2d` over two (`points`: a clip and where it lies),
    or a `Machine` (its `entry` state); a node's `speed`, its `sync` group, its place on the
    canvas (`at`).
  - **`states`** by id: a name (what `animator.state` will read), the `machine` it belongs
    to, the `pose` node it shows, its place on the machine's canvas.
  - **`transitions`** by id: `from` a state (none: from any state of the destination's
    machine), `to` one, `when` (a condition), `blend` seconds (0.25), `priority` (the
    smallest first), an `exit` time as a fraction of the source's clip (0: none), an `offset`
    into the destination or `keep_phase`, and `inertialize` in place of a cross-fade.
  - A clip's points refer to clips by name rather than to nodes: a blend of blends can come
    later as a node reference beside `clip`, without changing these lines.
  - `preview` names a model for the editor; the game binds a graph to the character
    playing it by the names of its clips and joints.
- **Lines at their defaults are left out**, found by writing each entry type's defaults once
  (no list of strings to keep in step with the fields); an entry with every field at its
  default keeps its first line, so it is read back. Sorted on write: a diff shows what
  changed.
- **`AnimGraphFile::problems`** names what does not hold together (a missing root, entry,
  machine, pose or state; a parameter a blend or a layer names and the graph lacks; a
  transition between two machines; a blend without clips; a transition that would go at once,
  with neither a condition nor an exit time), each with what it is (`transition t1`), for the
  editor's marks and the cook.
- **Conditions** (`engine_anim::Condition`): numbers, parameters, `true`, `false`; `+ - * /`
  and a unary `-`; `< <= > >= ==` and `~=` or `!=`; `and`, `or`, `not` (also `&&`, `||`, `!`);
  parentheses.
  - `not` binds looser than a comparison (`not speed > 1` is `not (speed > 1)`, unlike
    Luau's `(not speed) > 1`, which no one means), `and` tighter than `or`. A comparison
    chained (`0 < speed < 2`) is refused with a hint to join two with `and`.
  - Compiled once into stack operations over parameter slots; `holds(values)` runs them
    on a fixed array of 32 values: no allocation, no name looked up, no panic (a malformed
    program reads false). A condition deeper than 32 pending values is refused.
  - The triggers a condition names are listed once each (`triggers()`), to be used up when
    its transition goes. A blank condition always holds.
  - Errors carry a byte offset and a length, for the editor to underline as the text is
    typed: "no parameter speeed", "expected a value at the end", "≥ is not part of a
    condition" (spanning the whole character).
- **Tests:** the file written without its defaults (an all-default parameter keeping a line)
  and read back equal and written alike again; the layers in order, a machine's states, a
  transition's machine; each kind of broken reference named, and a graph that holds together
  naming none. Conditions: precedence and every operator spelling, triggers listed once,
  each fault with its place (Unicode spanning its character), the depth limit.
- **The scripts' calls** (ADR 0124's proposal: `animator:set`, `animator:trigger`,
  `animator.state`, `animator:in_window`) are the public scripting API, so they were put to
  the team (2026-10-30) before 5c needs them.

## As built (E12-4e-2, 2026-10-30): a character's joints posed in the view

- **A character's own clips beside its model's:** the Animation tab (and the inspector's
  Clips) list a character's model clips first, then the clips its animator has made in the
  editor (E12-4b left that for later).
  - Choosing a clip chooses its kind's tools: an own clip shows Once, Length, the tracks,
    keys and curves; a model clip shows its loop's seam, root motion, Mark Foot Plants, the
    picked joint's trail and P in place.
  - New Clip is offered for a character too. The new clip comes after the model's, chosen
    and held at its start.
  - The list follows the animator: a clip made, renamed or deleted rebuilds the own part.
    A steady frame compares in place and allocates nothing.
  - Choosing one kind stops the other playing: an own clip ends P in place, a model clip
    stops an own clip's ▶.
- **The picked joint's rings:** in Tab's joint mode, with one of the character's own clips
  shown, the picked joint gets rotation rings in its own axes (the gizmo's rings, drawn and
  hit as for an entity). The rings take the pointer before the joints do.
  - A ring dragged turns the joint (snapped by the rules, Shift and Ctrl as for the gizmo).
    Its turn in the world becomes a turn in its parent's space (`Rig::root_place` above a
    root), keyed at the playhead's frame as `@<joint>.rotation`.
  - The keyed angles are the Euler angles nearest the track's at that moment (or the
    joint's own turn without a track), so a key never turns a whole turn from its
    neighbours (`engine_app::euler_near`, now public).
  - The clip is edited live: the character follows the pointer. Letting go writes one undo
    step, "Pose <joint>". Escape puts the clip back as it was (the turn ends when the
    button is up); Play takes an unfinished turn back.
  - A playing clip stops at its frame when a ring is pressed.
- **K in joint mode** keys the picked joint's turn, as the pose has it, at the playhead
  ("Key Joint"). Outside joint mode K keys every track of the clip, as before.
- **The status bar** says what the rings and K do while a joint can be posed.
- **Also fixed:** the clip's time text ("frame 0 / 48") was written again only when the
  time changed, so choosing another clip at the same moment kept the old clip's frame
  count. It now follows the chosen clip and its length too.
- **Measured:** 0 allocations in 240 frames of a character's own clip playing in joint
  mode with the picked joint's rings laid out every frame (a new phase of the editor's
  clips zero-allocation test, after the asset database's one-off scan of the written clip).
- **Tests:**
  - **GPU, CesiumMan:** New Clip on him lists his model's clip and his own. Tab and a click
    pick a joint with a child, and the ring facing the camera dragged 60 pixels along
    itself turns it: the clip file gains its `@<joint>.rotation` track and the child moves
    11 cm. Undo takes the track away and puts the child back. K at frame 10 keys the joint
    there.
  - **The Clips:** a character's own clips follow its model's as its animator lists them,
    the new one chosen. Followed again with nothing changed, the choice stays.
- **Not yet:** moving a root joint (its translation) with arrows, keyed marks on the
  Skeleton's rows, rings for several joints at once, IK handles (E12-6).

## As built (E12-4e-1, 2026-10-30): joints keyed in `.anim`

E12-4e is taken in two steps: **4e-1** the joints' tracks in the file, played and previewed
(this one); **4e-2** posing a character's joints in the editor (joint mode's gizmo keying
them, K on a picked joint).

- **A joint's track** is named `@<joint>.<part>`: `@mixamorig:Hips.rotation`, the joint by its
  name in the model (dots and colons kept; the part follows the last dot), the part
  `translation`, `rotation` or `scale` (`engine_scene::joint_target`, `split_joint_target`).
  - Its keys are in the joint's parent's space, as the model's clips have them. A rotation
    is three Euler angles in degrees, as a field's (four numbers: a quaternion, as glTF
    has it), and a scale is one number (uniform) or three.
  - A clip may key fields and joints together. The fields' tracks and the joints' are laid
    out apart (`FieldClip::tracks`, `FieldClip::joints`), so the fields' bindings, the
    editor's sampler and its keying never see a joint as a field.
- **The pose:** a character's animator playing one of its own clips poses the joints it
  keys over the rest pose, the joints it does not key at rest (`FieldClip::pose_joints`,
  `Posing::local`). A clip keying no joint leaves the rest pose, as before.
  - A joint is found by its name once per model: each track remembers where it found its
    joint, checked by name before it is used, so characters of one model search nothing
    each frame. A joint the model lacks is left out.
- **Fades across clips of both kinds:** `animator:play(clip, fade)` blends the two poses
  whichever each one is: own into own, a model's clip into an own one and back
  (`Look::own_fade` beside `Look::fade`, `animator_pose` makes both). An own clip keying
  no joint fades as the rest pose.
  - An own clip moves nothing by root motion, so through a fade into or out of one the
    model's clip's motion fades with its weight (`RootMotion::blend` with `NONE`).
  - The own clips' lengths are told to the animator by their ticks, as before; the
    model's by the frame, as before.
  - An animator with own clips stays among the fields' ticks while it plays its model's
    clips. It used to drop out and be bound anew when it came back, which allocated at
    every switch; now going from an own clip to a model's and back allocates nothing.
- **The editor's preview:** the Animation tab showing an own clip on a character poses its
  joints at the playhead, held within the clip (its end shows the last keys, also for a
  clip that loops). It is let go when the tab, the clip or the entity changes, and the
  animator's pose comes back. A key changed in the tab is drawn at once
  (`Assets::set_anim_clip` reposes the characters showing that file); so is a file
  changed on disk.
  - The dope sheet and the Curves view list the joints' tracks by their targets, and their
    keys are picked, moved, copied and curved as any track's.
  - **K** keys the clip's joints too, with the values the clip has at the playhead; the
    message counts fields and joints ("2 fields and 3 joints keyed at 0.50 s").
- **Measured:** 0 allocations in 654 frames with a character playing an own clip that keys
  three joints (and one its model lacks), fading into its model's clip and back every 100
  frames. That is a new character in the skinned models' zero-allocation test, beside the
  door with its two own clips.
- **Tests:**
  - **Targets:** split at the last dot, names with dots and colons kept; a field's target
    and an unknown part are not a joint's.
  - **The pose** (CesiumMan): the keyed joint turned 90 degrees at the playhead's end, the
    other joints at rest, its child moved and the root's place kept. An animator's time
    half way gives 45 degrees, and 1 s wraps to the start of a looping clip.
  - **Fades:** two own clips at half weight give 45 degrees between 0 and 90. One keying
    no joint into one keying 90 gives the rest pose's turn half way to it. Two keying none
    give the rest pose's palette.
  - **The editor:** K keys a joint's track at the playhead with the clip's 45 degrees and
    the field beside it, in one step; undone, the file is as before. The fields' sampler
    sees the position alone.
  - **GPU, the zero-allocation test:** the waver's arm's child 0.13 m from its rest place
    at a third of his clip.
- **Not yet:** posing joints in the editor (E12-4e-2); joints' keys on a model's clips
  edited (an imported clip is read-only, as before); masks and layers that combine an own
  clip with a model's (graphs, E12-5).

## As built (E12-4d, 2026-10-30): curves

- **Dope Sheet | Curves** in the Animation tab's header (and C in the tab) switch a clip made
  in the editor's tracks' rows for the Curves view (Unity's switch, UX note 8).
  - It draws the tracks of the picked keys, or every track with none picked.
  - Each number of a track is a line in its color (x, y, z as the gizmo's axes, w grey),
    with its keys as points on it.
  - It is fitted to the values drawn with a margin; the values at its top and bottom are
    written there.
- **How a line runs:** a smooth stretch through samples about 4 pixels apart (the clip's own
  monotone cubic, `engine_app::TrackCurve`), a linear one straight, a step flat to the next
  key and up.
- **A key's point:** a press picks its key (Ctrl adds it or takes it away). A drag past 3
  pixels moves the picked keys in time by whole frames (Shift: freely), as the dope sheet
  does, and the pressed key's number by the height dragged.
  - The view's range is held while it drags, so the scale does not move under the pointer.
  - It is one step ("Move Keys"); Escape puts the clip back.
  - Points one on another (three numbers at 0): the one drawn last is pressed.
- **The picked keys' curve:** Smooth, Linear and Step show in the header while keys are
  picked, the one they share lit. A key's curve is how the clip goes from it to its next
  key, as the file always had it. One step ("Key Curve"); also from the palette ("Picked
  Keys Smooth" and so on).
- **Lines in the UI:** `engine_ui` gained a node with a line through points given as
  fractions of its rectangle (`Ui::line`, `Style::line`, `DrawCmd::Line`), so a curve is
  laid out, clipped and drawn in order with the panel. The editor paints a piece as a
  line quad; the points are kept for the frame, with nothing allocated in steady state.
- **Measured:** 0 allocations in 240 frames of the Curves view, the door's clip playing (the
  editor's zero-allocation test gained the phase).
- **Tests:**
  - The lines: a step flat to just before the next key, a linear stretch straight, a smooth
    one sampled; the range fitted with a tenth more each way, or the one given kept. The
    picked keys' shared curve, and setting it.
  - The editor's: C shows every track's six numbers and twelve points, then the picked
    key's track alone; Step through the palette's command; a dragged point written as one
    step; both undone.
  - The door's GPU test, with the pointer: Curves clicked, a key's point dragged up 30 pixels
    raises one number and moves nothing in time, Step clicked writes the key's curve.
  - `engine_ui`'s: a line drawn piece by piece where its node is laid out, and gone the next
    frame.
- **Not yet:** tangent handles (a smooth key is the clip's own monotone curve, ADR 0124's
  E12-4a), framing and zooming the view, values snapped; joints keyed in `.anim` (E12-4e).

## As built (E12-4c, 2026-10-30): the dope sheet's editing

The Animation tab edits a clip made in the editor's keys on its tracks. The edits are
functions of the clip (`dope.rs`: `move_keys`, `delete_keys`, `copy_keys`, `paste_keys`,
`keys_in`); each one is held, given to the player and written as one undo step, as the
events' edits are.

- **Picking:**
  - A press on a key's mark picks it; Ctrl adds it to those picked or takes it away.
  - Picked keys are drawn larger and in the text's color: by size, not color alone.
  - A key is known by its track's place among the clip's tracks and its own in the track.
    The picked ones are let go when another clip shows, and dropped when undo takes their
    keys away.
- **A box:** a press on a track where no key is starts a box, drawn over the tracks; let go,
  it picks the keys inside it. With Shift or Ctrl the box adds to those picked. A click
  picks none.
- **Moving:** past 3 pixels, a drag moves the picked keys together, by whole frames, and
  with Shift freely (the owner's rule, UX note 13).
  - They are held within the clip by the first and last of them.
  - A key moved onto another of its track takes its place.
  - The drag follows the pointer each frame (a mark's place changes as keys pass one
    another) and ends with the button: one step, "Move Keys". Escape puts the clip back.
- **Delete** in the tab deletes the picked keys ("Delete Keys"); with none picked, it
  deletes the picked event, as before. A track left without keys goes with them.
- **Ctrl+C and Ctrl+V** in the tab copy the picked keys, each keeping its distance from the
  first, and paste them from the playhead's frame ("Paste Keys").
  - They paste into the tracks of the same names, in this clip or another; tracks missing
    are made, and the clip grows to hold the keys.
  - The pasted keys are picked.
- **The length** of a clip made in the editor is typed in the header after Once (seconds,
  sums too, as any typed number): whole frames, at least one ("Clip Length"). Keys past it
  stay in the file, so a longer length brings them back.
- **The tab's facts follow the file:** a clip made in the editor's length, frames and track
  count are read again when its keys or length change; before, they were read once, when
  the tab showed it.
- **Tests:**
  - The dope sheet's: keys move together, held in the clip and replacing what they land on;
    deleting takes an emptied track; copied keys paste at the playhead keeping their
    distances (and curves) into this clip or one without the tracks; a box picks what it
    covers.
  - The editor's: a move written when the button is up; a delete taking the scale's
    track; copy and paste at frame 3 picking the pasted keys; the length typed as "1 + 1";
    each one step, the four undone back to the file.
  - The door's GPU test, with the pointer: the last key's mark picks it and drags it from
    the end to frame 15 in the file; a click on an empty track picks none; a box picks it;
    Delete takes it away.
- **Not yet:** keys' curves (the Curves view, E12-4d), and joints keyed in `.anim` (E12-4e).
  Shift+click to extend, `,`/`.` and Alt with them, and F to frame the picked keys (UX note
  8) come with the Curves view.

## As built (E12-4b-2, 2026-10-30): keys (K, ◆, editing a field the clip moves)

- **The editor's frame drives the clip's fields** (`keying::Driving`, held by the
  `History`): while the Animation tab shows a clip made in the editor on its entity, held at
  the playhead or playing, the frame begins with each field its tracks key holding the
  clip's value there (`History::drive`, sampled by `engine_app::ClipFields`) and ends with
  the scene's values back (`History::undrive`). The inspector shows the clip's values and a
  drag starts from them, as Unity's animation mode drives its properties.
  - **The scene keeps its values:** every record undo takes has the scene's values for
    those fields (`History::snapshot`), and so has a scene or a prefab written in the frame
    (`History::scene_values`, `drive_back`). Undo, redo, a revert or a cancel that puts an
    entity back keeps what it put there as the scene's value and shows the clip's again.
  - **An action that changes one keys it** at the playhead's frame, in the action's own
    step: the clip's text before and after is a file's change (ADR 0120's `change_file`).
    A drag is one step with its keys, Escape's revert takes them away, and undo hands the
    clip's text back. A rotation keys the Euler angles nearest the ones its track has, so
    no key turns a whole turn from its neighbours.
  - **When keys reach the file:** they are held and given to the player at once; the file
    is written once no drag or keyboard transform goes on.
  - **While the clip plays** an edit of its fields is not kept: they hold the clip's values
    each frame. Stop it to key.
  - **The playhead lands on frames:** a clip made in the editor's playhead lands on its
    frames (a scrub, ■), where keys go.
- **K** (Animation: Key the Clip's Fields) keys each field the clip moves at the playhead's
  frame with the value it shows there.
  - A clip without tracks gets keys on position, rotation and scale (Blender's default set).
  - It is one step ("Key Fields"), and a playing clip stops there.
- **◆** follows each field's reset button while such a clip shows on the inspected entity
  (`shell::KeyMark`). A click keys that field alone ("Key Field"). It is:
  - filled where a key lies at the playhead;
  - outlined where the clip moves the field (a track of `scale.y` marks the scale);
  - dim where the field can be keyed (one to four numbers, or a rotation);
  - absent for the rest.
- **Text layouts** of numbers that change every frame (the inspector following a playing
  clip, six new texts a frame) need layouts back sooner than the 2 s a layout waits to be
  laid out into again: with fewer than 64 spare (`paint::SPARE_LOW`) after a frame that laid
  texts out, those long out of use come back first, then those unused this frame and the
  last, then new ones are made up to 64. Taken back only at 2 s, the spare ones hung near
  none and a frame with one more new text than came back allocated (4 to 8 allocations in
  240 frames, more under the gate's load); now 0, also under load and on both GPUs.
- **Tests:**
  - The history's: a drag's two frames key 60 then 70 degrees in one step, with the scene's
    turn back each frame; undo and redo hand the clip's text back; a move is recorded with
    the scene's turn, as is a scene written in a frame; a revert keys nothing, and nothing
    is keyed while the clip plays.
  - The editor's: the turn is marked ◆ and shown as the clip has it; an edit keys it at
    frame 15 with the scene and its text untouched, one step that undo takes back; K on a
    new clip keys the three fields; ◆ keys the position alone at frame 10.
  - The sampler's: fields are found on an entity and on a child, and a turn edited past
    half a turn keys 200, not -160.
- **Not yet:**
  - ◆ for a child's fields: select the animated entity; its children's tracks come from
    the file, or from the dope sheet (E12-4c).
  - A clip's length is its file's (1 s for a new one) until the dope sheet.

## As built (E12-4b-1b, 2026-10-30): New Clip, renaming and deleting clips

- **New Clip** (`animation.new_clip`, the tab's button for a selected entity without a
  rig): an `.anim` (the defaults: a second at 30 frames a second, no tracks) in the dock's
  folder, named `<entity> <clip>.anim` with a number after it when taken (as New
  Material's), written and scanned at once (`write_own_file`).
  - It goes among the entity's animator's clips as `clip` (`clip 2`…), the animator playing
    it, an `Animator` added when it has none, as one undo step ("New Clip"); undone, the
    file stays.
  - The tab opens on it, its name field focused (`clip_autofocus`, cleared once drawn).
- **Renaming:** the header's name field for a clip made in the editor; Enter renames its
  key among the animator's clips and the animator's `clip` where it was that one ("Rename
  Clip"). The file keeps its name; a name taken is refused with a message.
- **Deleting:** ✕ or `animation.delete_clip` takes the chosen clip from the animator (its
  `clip` moves to its first other one), one step ("Delete Clip"); the file stays.
- **Team decision (2026-10-30, the owner's «а»):** K keys the fields the shown clip
  already moves at the playhead; a clip without tracks gets keys on the entity's position,
  rotation and scale (Blender's default set). ◆ beside an inspector field keys that field
  alone, and editing a field the clip moves keys it at the playhead (auto-key; no record
  mode, question 8a). Taken: K's other ways (only the fields changed since the last key,
  as Unity's Shift+K; a list of fields to tick) cost more steps for the common case.
- **Tests:** New Clip on the box without an animator writes `props/Box clip.anim` (an
  `Anim` asset at once), gives it an animator playing `clip`, undone (no animator, the
  file kept) and redone; renamed `open`; a second New Clip makes `Box clip 2.anim` and is
  chosen; deleted, the animator keeps `open` and plays it; undone, both again; the steps
  named "New Clip", "Rename Clip", "New Clip".

## As built (E12-4b-1a, 2026-10-30): clips made in the editor in the Animation tab

- **The preview** (`Player::preview_anim(Some((entity, file, time)))`, asked each frame by
  the editor): while the world stands still, the player sets the clip's fields at that
  moment just before the scene is propagated and extracted, and gives them their values
  back (`KeptField`, read by `FieldRef::read` the first time each field is set) as the
  frame's presentation begins, before the game or the editor reads or writes the world.
  - So the inspector, undo, saves, autosaves and Play's snapshot only ever see the
    scene's own values. Godot's previews write into the scene (hence its RESET clip);
    Unity loses an edit not keyed when the playhead moves. Here nothing is written.
  - The preview binds the entity as an animator does (`FieldClips::preview`, its own
    bindings kept), clamping the playhead into the clip: a looping clip's end shows its
    last keys.
  - Transforms propagate with the ticks (64 a second): the preview moves as the editor's
    own edits do.
- **The tab:** an entity without a rig whose `Animator` has clips gets the Clips' view of
  them (`ClipsView.own`: each clip's file; `ClipFacts::own`: length, frame rate, tracks).
  - The animator's own clip is chosen first; ↑/↓, the ruler, ←/→, Home/End and ▶ work as
    for a character's clips. A clip that plays runs on the frame's time
    (`ClipsView.playing`); a step or a scrub holds it.
  - It is shown while the tab is open, the game not playing, and the clip plays or holds
    a moment; shutting the tab, choosing another entity or Escape gives the scene back.
  - **Tracks** (`AnimationView.tracks`, `TrackRow`): a row per track under the events,
    a mark per key; root motion's toggles and Mark Foot Plants are not shown for such a
    clip (`AnimationView.own`).
- **The clip's own file holds its events and loop:** the events' document (`EventsDoc`)
  holds a `.meta` or an `.anim` (`Held`), so the events row, its sounds and Once work on
  both. Each change is written to the `.anim` and is one undo step; undo's text goes back
  to the held document, or to the player through `anims_due`.
- **Not yet:** an entity with a rig and clips of its own lists its model's clips only;
  New Clip, renaming and deleting a clip (E12-4b-1b); keys (E12-4b-2).
- **Tests:**
  - **Engine:** a preview at 1 s and 0.5 s writes 90 and 45 degrees and keeps each field
    once; put back, the scene's own values return.
  - **Player (GPU):** the door drawn at 90 then 45 degrees (`GlobalTransform`) while its
    `Transform` stays the identity; stopped, drawn as the scene has it.
  - **Editor (unit):** a door's two clips listed, the animator's first; its tracks, events
    and Once shown, Across and Mark Foot Plants not; Once written to the `.anim` and taken
    back; ▶ plays, End holds at 1 s, ↓ chooses the second clip (2 s).
  - **Editor (GPU):** Alt+N, a press at the ruler's end: drawn at 90 degrees, its
    `Transform` and the saved scene the identity; the tab shut, drawn as the scene has it.
  - **Zero allocations:** the editor's frames with the door's clip playing in the tab
    (tracks drawn, the player previewing it every frame): 0 in 240 frames, beside the
    character in place and the dock's preview.

## As built (E12-4a, 2026-10-30): clips made in the editor, `.anim` files

E12-4 is taken in five steps: **4a** the files and their playing (this one); **4b** the
editor's own clips (New Clip, their tracks in the Animation tab, scrubbing without writing
the scene, K and ◆, a key made by editing an animated field); **4c** the dope sheet's
editing (select, box, move by whole frames and Shift freely, copy and paste, delete);
**4d** curves (the Curves view, step, linear and smooth keys); **4e** joints keyed in an
`.anim`.

- **The file** (`engine_scene::AnimFile`, `.anim`, `AssetKind::Anim`, its id inside, as
  the other files of ours): `length`, `rate` (the editor's grid, 30 frames a second),
  `once`, `events` (as an imported clip's `ClipEvent`s) and `tracks`.
  - **One track a field, keyed by what it moves:** `Transform.rotation`, or
    `Lamp:PointLight.color` for a field of an entity below the animated one, named by
    its path of names (`Arm/Lamp:`), as `Tween`'s targets.
  - **A key** is its time, as many numbers as the field and its curve (`Smooth` unless
    it says `Step` or `Linear`; the curve goes from that key to the next).
  - **A rotation is keyed as the inspector shows it:** three Euler angles in degrees
    (pitch, yaw, roll, applied yaw first). A door's 90 degrees reads `[0, 90, 0]`, and a
    fan turns past 180 between two keys, which a quaternion cannot.
  - Lines at their defaults are left out, and the tracks sort by name, so a merge sees
    the tracks a change touched.
- **Smooth keys** use a monotone piecewise cubic (Fritsch and Butland's weighted harmonic
  slopes, as PCHIP): flat at the ends and where a key is higher or lower than both its
  neighbours, so a curve never overshoots its keys. Two keys ease in and out.
- **The `Animator`** gains `clips` (hidden in the inspector, saved, in scenes): the clip
  files it plays by name, besides its model's, a name there winning. `with_clip(name,
  file)`, `plays_own_clip()`. A door's `open` and `shut`, played by
  `self.Animator:play("open", 0.2)`.
  - `engine_runtime` now depends on `engine_scene` for `AssetRef`; nothing goes the
    other way.
- **Played each tick** right after the animators move on (`Assets::animate_fields` in the
  player's tick, `engine_app::field_clips`), so the game, scripts, saves and the network
  see the tick's values; a moved transform is drawn between ticks as every one is.
  - An animator binds a clip once: each track's entity found by names and its field by
    `engine_runtime::FieldRef` (the reflected component and offset, the code `Tween`
    used, now shared). It keeps the bindings of every clip it played, so switching clips
    binds nothing.
  - The bindings lie in the animators' order, the same tick after tick, so a tick looks
    nothing up; they are lined up again only when animators come or go.
  - The clip's length and `once` go back to the animator as a model's clip's do.
  - Through a fade, a field both clips key blends by the fade's weight (Euler angles
    blended, then turned into the rotation); one only the clip fading out keys keeps its
    value until the fade ends.
  - Events fire as an imported clip's, over the tick's span, the heavier clip's in a
    fade, appended to `AnimationEvents` after the models'.
  - A skinned model's animator playing one of its own clips poses nothing: the model
    rests (combining the two is a graph's layers, E12-5).
- **Files:** read from the path a scene's reference gives once, again when the file
  changes (`Player::asset_changed`, `Assets::reload`); `Assets::set_anim_clip(file,
  Some(&anim))` holds the editor's version in place of the file.
- **Nothing is written while the editor edits** (the world stands still): a door shows
  the scene's values. Previewing an own clip without writing the scene is E12-4b's.
- **Cost** (ref-1, release): 1,000 doors, each turning (three smooth Euler keys) with its
  lamp's range stepping, 69.5 µs a tick, 70 ns a door (116 µs while each tick looked the
  bindings up in a hash map and borrowed every animator).
- **Tests:**
  - **The file:** written and read back with its defaults left out; keys kept in time
    order, a key at a keyed time changed; targets split.
  - **Tracks:** steps hold, lines, the ends held; smooth keys ease (45 degrees halfway
    between 0 and 90) and never overshoot a peak or fall while rising.
  - **A bare world:** the door turns 45 degrees at half a second and 90 after, its lamp's
    range steps, the clip played once holds, its event fires once; a fade to `shut`
    turns it back steadily; an animator playing a clip it does not own is left alone; the
    editor's edit is followed the next tick.
  - **GPU, the player:** a scene saved with the door reads `door open.anim` from its
    reference; 45 and 90 degrees, the lamp's step, one event in `AnimationEvents`; the
    file written anew and reported changed, the door follows it.
  - **Zero allocations:** the player's skinned frames with a door switching between two
    of its own clips by fades: 0 allocations in 702 frames, 15 of its events heard.

## As built (E12-3e, 2026-10-30): each clip's loop and root motion settings

- **The `.meta`** (`ClipImport`): `once`, plus `root_across`, `root_up` and `root_turn`
  beside the events. The default is a loop with the ground's root motion (across, turn);
  question 7 has them in the model's `.meta`.
  - `ClipImport::is_plain` says when an entry says nothing, and the editor drops such
    entries from the file.
  - An older `.meta` with events only reads the defaults.
- **Once:** the player tells the `Animator` each frame whether its clip (and the one
  fading out) plays once, as it tells it the clips' lengths (`hold`, `from_hold`).
  - `advance` then holds `time` within the clip instead of wrapping it (backwards, at its
    start).
  - Frames between ticks show the end, not the start (`between` clamps instead of
    wrapping).
  - `play` lets the next clip loop again, the one fading out keeping its own.
- **Root motion's axes:** `Look` carries each of its two clips' axes, and
  `Rig::motion_mode_of(axes)` reads root motion along them.
  - The same axes take the motion out of the pose (`Posing::local`) and move the character
    (`tick_motion`, `clip_motion`), so a jump with `root_up` rises as the entity and stays
    level in the pose.
  - The preview's floor still uses the default axes.
- **Settings and events share one table** (`ModelEvents`, `ClipPlay`), read from the
  `.meta` and swapped by `Assets::set_clip_events`.
- **The editor:** the Animation tab's header has "in the game: Once · root motion:
  Across Up Turn", lit as the `.meta` says. Each toggle is an undo step through
  `EventsDoc::edit_clip`, the events' document now a clip entry's.
- **Tests:**
  - **Unit:** a clip played once holds its end (frames between show the end; backwards
    it holds its start; the next clip loops); the `.meta` round trip with a clip of
    settings and no events.
  - **The tab:** Once and Up toggled into the `.meta`, the steps undone back to no entry
    and redone.
  - **GPU, the walker:** by default `moved` has no rise and fall. Once with `root_up`, its
    Walk holds at 1.0 s, `moved` reports the hips' bob (3.9 mm a tick at most) and then
    nothing. Turn without `root_turn` keeps the entity facing as it stood. The events
    and root motion tests pass as before.
- **E12-3 is done:** the timeline, events with sounds, trails and speed curves, Mark Foot
  Plants, and the clips' settings.
- **Left:** the preview's own loop toggle still loops a clip set to once (the preview is
  for watching).

## As built (E12-3d, 2026-10-30): Mark Foot Plants

- **The command** (`animation.mark_foot_plants`, and the tab's button beside + while no
  event is picked) works on the chosen clip, of a character or of the dock's model.
  - It puts a `footstep` event, data `left` or `right`, where each foot touches down.
  - It replaces the footsteps it marked before (name `footstep`, data `left` or `right`),
    as one undo step written to the `.meta`.
  - It runs in the next `clips_frame`, where the rig is.
- **The feet** (`clip_events::find_feet`): the first joint named for a foot on each side
  (`foot`, else `ankle`; the side from `left`/`right` or an `L`/`R` part of the name).
  - **The Fox:** `b_LeftFoot01_017` and `b_RightFoot01_021`.
  - **Else:** each named side's lowest joint in the rest pose (CesiumMan:
    `leg_joint_L_5`, `leg_joint_R_5`).
  - **Else:** the lowest joint and the lowest one a tenth of the skeleton's height apart.
- **The touch-downs** (`clip_events::foot_plants`): where the foot's path (`joint_path`,
  heights along `Rig::up`) comes down into the lowest fifth of its height's range.
  - Speed is no guide: in a clip made in place, a planted foot slides back nearly as fast
    as it swings. A first speed criterion marked three footsteps for two on a synthetic
    walk, so it was dropped.
  - A loop leaves out its last sample (its first again), and does not mark its start when
    the foot was already down before its end.
  - **Engine side:** `Rig::up` and `Rig::rest_places` (the rest pose's places).
- **Tests:**
  - **Unit:** a walking foot's touch-downs, one a cycle, once across a loop's end, none
    for a foot that never lifts; feet found by name (the Fox's), by each side's lowest
    (though a joint of the same leg lies well apart), and by geometry.
  - **GPU, the Fox:** Walk gets a right footstep at 0.042 s and a left at 0.542 s.
  - **GPU, CesiumMan:** one left and one right in his 2 s walk.
- **Left:** clips picked together for one run; foot locking or IK from the plants.

## As built (E12-3c, 2026-10-30): a picked joint's trail and speed curve

- **The path:** `Rig::joint_path(clip, joint, out)` gives the joint's place in the model's
  space at each of the clip's samples (`Posing::joints` a sample, the root motion left
  in). The editor asks for it again only when the character, its rig, the clip or the joint
  changes (`JointTrail`).
- **The trail:** while the Animation tab shows the character's clips with a joint picked
  in the Skeleton, the path is drawn in the view where the character stands, as a line in
  front of everything. Each frame gets a tick 3 cm long, so a pop shows as a long gap. The
  trail is magenta, apart from the selection's orange outline, the skeletons' white and
  the colliders' colors.
- **The speed curve** (`speed_curve`): each step's length over the longest, a bar a frame
  in a row under the events' strip, aligned with the ruler's frames. The longest bar is lit
  in the warning color. The tab's row adds the joint, the longest step, its frames and its
  speed; a joint that never moves "stands still in this clip". A press or a drag on the
  bars scrubs, as the ruler does. The tab grows to 182 pixels while it has the curve.
- **Tests:**
  - **Unit:** the curve, with a pop named by its frames and speed, and a still joint.
  - **The walker:** Hips' path through Walk, 31 places a metre along +z, always forward;
    none for a clip it has not.
  - **GPU, CesiumMan:** with a joint picked in the view (`leg_joint_L_2`) and the tab
    open, 97 trail lines are drawn: 48 steps and 49 ticks for his 49 samples. There are 48
    bars, and the label reads "the longest step 5.2 cm, frame 4 to 5 (1.25 m/s)". A press
    on the curve's middle gives 1.000 s of 2.000 s. With the tab shut, there is no trail.
  - **Screenshot:** `ENGINE_EDITOR_TRAIL_PNG` (the knee's loop in magenta).
  - 0 allocations unchanged (both builds).
- **Left:** the dock's model (it has no Skeleton to pick from); each joint's own
  compression error beside its curve; Mark Foot Plants, which these paths feed.

## As built (E12-3b-3, 2026-10-30): event sounds in the editor's previews

- **Which events play:** each frame, the span of the shown clip a preview played is found.
  For the dock's model, that is the span its floor already walks by. For a character
  played in place (P), it is the span of the Clips' character. Its events go through
  `clip_events::passed` (forward, with loops), and the last one with a sound plays.
- **Where it plays:** through the dock's preview slot (`PreviewAsk::Play`, the editor's bus
  straight to the master, one at a time); the dock shows it when the sound is a project
  asset. This is the UX note's "sounds play in every preview".
- **Held with the Clips:** the Clips' model file's `.meta` is held whenever Clips show, not
  only while the Animation tab does, so the previews hear its events.
- **Tests:**
  - **Unit:** the spans a preview's frame passes (across the loop, never backwards).
  - **A frame:** one that passed an event with a sound asks for it, across the loop too.
    One that passed only an event without a sound, or none, asks for nothing.
  - **GPU, the Fox in the dock:** a click on the mark picks it, and `step.wav` is typed
    into its sound field (written to the `.meta`). Three frames back and Space: the
    preview passes it and plays `step.wav`.
  - 0 allocations unchanged (both builds).
- **Left:** a sound dropped from the dock onto the sound field; sounds that overlap
  rather than cut each other (the preview slot plays one).

## As built (E12-3b-2, 2026-10-30): the Animation tab's events

- **The strip** under the ruler (`AnimationView::events`): a mark and a name per event of
  the chosen clip, the picked one lit.
  - A press picks a mark. A drag past 3 pixels moves it frame by frame, keeping where it
    was grabbed. The move is held in memory and given to the player; letting go writes it
    as one step.
  - A press on the strip off the marks lets the pick go.
- **Adding and removing:**
  - E (`ANIM_KEYS`) or + adds an event at the playhead's frame, named "event", picked, its
    name field focused.
  - Delete or the row's ✕ removes the picked one.
- **The row under the strip:** the picked event's name, data and sound in text fields,
  committed by Enter, Tab or a click elsewhere. An empty name keeps the old one.
- **`EventsDoc`** (`engine_editor::clip_events`): the Clips' model file's `.meta`, held
  while the tab shows its clips (a character's model file, or the one picked in the dock).
  - Each edit keeps the events in time order, drops a clip left without events, and
    writes the `.meta` at once.
  - It records the text before and after with `History::change_file`; undo and redo put
    the text back (`put_back_files`).
  - A file the tab no longer holds is read for the step, and its events go to the player
    all the same.
  - The player gets them through `Assets::set_clip_events` in the next `clips_frame`.
    A `.meta` write is no change of the file (`AssetDatabase::scan` compares the file
    itself), so nothing is loaded again.
- **Fixed on the way:** E12-3a laid the ruler's numbers and playhead out 600 pixels wide.
  `Ui::rect_of` sees only the frame being built, so it never found last frame's ruler.
  The app now passes the ruler's last width in `AnimationView::width`, read before the
  frame begins.
- **Tests:**
  - **Unit:** the doc's edits, writes and put-backs, order kept and frames snapped.
  - **The tab:** E at 0.51 s gives an event at 0.5 s with its name typed at once, then a
    rename; dragging the mark a quarter of the strip lands it at about 0.75 s. The steps
    are "Add Event", "Rename Event" and "Move Event", and undo and redo put the `.meta`
    back. Delete removes the clip's entry, and undo brings it back. The ruler's last
    number sits near its right end.
  - **GPU, the Fox in the dock:** E adds an event at the playhead (1.71875 s, frame 165 at
    96 Hz), written and drawn. Escape, then Delete, removes it, and Ctrl+Z brings it back.
  - **Screenshot:** `ENGINE_EDITOR_EVENTS_PNG`.
- **Left:**
  - event sounds heard in the editor's previews (the UX note's "sounds play in every
    preview");
  - a sound dropped from the dock onto the sound field;
  - a picked joint's trail and speed curve;
  - Mark Foot Plants;
  - the clips' loop and root motion settings.

## As built (E12-3b-1, 2026-10-30): clip events

- **The `.meta`** (`engine_scene::ModelImport::clips`, as question 7 has it): a
  `ClipImport` per clip with events, found by its name in the file (an unnamed one as
  `Clip 2`), its `ClipEvent`s each a time, a name, data (text for whoever hears it) and a
  sound's path. The writer drops lines equal to the defaults' by path, not by place: a list
  with items writes more lines than the defaults' empty one, and the pairing by place cut
  everything after it. `AssetMeta` and `ModelImport` are no longer `Copy`.
- **Firing** (`engine_app::clip_events`, `Assets::fire_events`): after `move_roots` in each
  tick, every character fires the events its clip passed over the tick's span, `[from, to)`
  forward and `(to, from]` backwards, every loop between (64 at most). Through a cross-fade
  the clip that weighs more fires (the clip fading out until the fade passes half), as the
  ADR has it; sync groups' leaders come with the graphs (E12-5).
- **`AnimationEvents`** (engine_runtime, a resource the player keeps): the tick's events
  (entity, name, data; names shared as `Arc<str>`), characters in entity order. It keeps
  its room between ticks rather than a fixed ring: no event is dropped, and a steady game
  allocates nothing. `ticks()` tells one tick's events from the next.
- **Scripts:** `on_animation_event(self, name, data)` on the character's instances, in the
  next tick's script stage after the contacts, each tick's events once; names made into
  Luau strings once each (`data` nil when empty). Windows (`in_window`) come with the
  graphs.
- **One hook (team decision, 2026-10-30):** the UX note's other way, an event calling the
  function of its name as `send` does (`Steps.step(self, data)`), is not taken now: one
  hook, seen in the types and completed as typed, keeps names out of the API. It can be
  added beside the hook later (about an hour) if `on_animation_event`'s `if name == …`
  grows tiresome.
- **Sounds:** an event's sound spawns an `AudioSource3D` despawned at its end where the
  character stands (not while the network re-simulates); it follows nothing.
- **Live edits:** `Assets::set_clip_events(file, clips)` swaps a file's events in place of
  its `.meta`'s (the editor's row, E12-3b-2); a reload reads the `.meta` again.
- **Cost** (ref-1, release, the Radeon's frame thread, 1,000 walkers each passing three
  events a loop): 17 µs a tick, 17 ns a character, with the characters' list made again
  only when the skinned models change (47.5 µs while it was collected, sorted and looked
  up every tick); 0.2 µs without events.
- **Tests:**
  - the spans (forward from the start, across the loop, two and a half loops, backwards,
    paused, a moment on a tick's border fired once) and clips found by name or number;
  - the `.meta`'s round trip with nested events;
  - walkers firing their Walk's events into `AnimationEvents` over 128 ticks: forward,
    backwards and at three times exactly as played, a restart under a half-second fade
    never doubling a step, the left step's sound standing in the world, and events set in
    place of the file's firing from the next tick;
  - a character's script hearing each tick's events once, data nil when empty;
  - 0 allocations: skinned characters firing eight events a loop, and ten scripts hearing
    two events a tick (both builds).
- **Left (E12-3b-2, done):** the Animation tab's events row.

## As built (E12-3a, 2026-10-30): the Animation tab's timeline

- **The tab** (the bottom bar's fifth, Alt+N, "View: Animation") shows the Clips' chosen
  clip (`AnimationView`). It works for the active character, or for the model file picked
  in the dock.
  - **Its header:** ↑/↓ to the clips before and after; whose clips they are (the file's
    name or the character's, found once per Clips) and the clip; ▶ or ■, the loop, the
    speed and the time, as the Clips have them.
  - **Its ruler:** numbers as far apart as they read (`ruler_step`: the fewest of 1, 2, 5,
    10… frames that leave 48 pixels between two), laid out by the width it had the frame
    before, the playhead along it. A press or a drag scrubs through the Clips' own
    `scrub_clip`.
  - **Its row:** whose clips they are, their skeleton's joints and the clip's sample rate.
- **Keys** (`ANIM_KEYS`, after a press in the tab, a hover region): ← and → step a frame,
  Home and End go to the first and last frames (`step_frame`: the clip stops there, held,
  as a scrub holds it), Space plays or stops.
- **One playhead:** the tab is another view of `ClipsView`, so the Clips section and the
  tab show the same clip, time and play.
- **Nothing allocates while it draws:** the ruler's numbers are written into a buffer on
  the stack, and its texts are the Clips' own (made once per Clips or per clip).
- **Tests:**
  - the ruler's steps, and frame steps held at the clip's ends;
  - Alt+N's command opening and closing the tab;
  - the Fox in the dock: Alt+N, a press on the ruler's middle puts the playhead at
    1.708 s of Survey's 3.417 s, and → steps one frame (to 1.719 s, at 96 Hz);
  - 0 allocations in 240 frames with the tab open while a picked file's clip plays (both
    builds).
- **Left (E12-3b on):** events (E adds one) and their sounds, a picked joint's trail and
  speed curve, Mark Foot Plants, and the clips' loop and root motion settings in the
  model's `.meta`.

## As built (E12-1b-3b, 2026-10-30): two clips side by side

- **The model preview's second model** (`ModelPreview::beside`): a second clip's moment of
  the same model.
  - It has its own instance of the skin, made the first time one is asked for and kept
    with the preview (`Assets::preview_beside`), posed as the first is (its root motion
    taken out with a floor).
  - The two stand apart along the frame's right on the ground (the previews' slant turned
    22.5°), 1.25 times the framing's reach on each side of the model's place. Each turns
    about its own place, and the camera frames the pair.
  - The floor stays the first clip's.
  - Until the second is made and posed the preview draws nothing new, so it never shows
    half a pair.
- **The editor:**
  - a Ctrl+click on a picked file's clip row plays it beside the chosen one, at the same
    fraction of its length, so a walk and a run are compared step for step;
  - its row is marked in the paler selection colour;
  - a Ctrl+click again, or on the chosen one, takes it away, and choosing it makes it the
    chosen one;
  - a character's Clips have no second: a Ctrl+click there chooses.
- **Tests:**
  - on both GPUs, the generated walker: the second model changes 5,581 pixels of the
    picture, its own turn 2,230 more, and back to one model the picture is the first;
  - the Fox in the dock: a Ctrl+click on Run puts it beside Survey (31,877 pixels of the
    inspector change), Survey still chosen;
  - the Ctrl+click's rules;
  - 0 allocations with the pair posed every frame (both builds).
- **Left:** the root's path and facing drawn on the floor. The preview draws meshes only,
  so drawing them needs lines in the preview's view or a strip mesh made per clip. It is
  kept for the Animation tab (E12-3), which draws a picked joint's trail anyway.

## As built (E12-2c, 2026-10-30): root motion

- **`Animator::root_motion`** (saved, shown): `RootMotion::InPose` (the default), `Move`
  or `Report`. `moved` and `turned` (not saved) hold the last tick's motion.
- **Where the motion is read** (`Rig`, made with the skin):
  - the root is the skin's topmost joint;
  - the motion is read in its parent's space, with the frame from there into the mesh's
    space (the rest pose's);
  - up is the file's, glTF's +Y in its scene. The skeleton holds every ancestor of its
    joints, so its space is the scene's. An armature or mesh node turned otherwise (Khronos'
    CesiumMan stands Z up under a turned node) still crosses the scene's ground. The mesh's
    own +Y would have read its hips' bob as ground.
- **`engine_anim`:**
  - `RootMotionMode` gains `up` (+Y by default): horizontal, vertical and yaw are measured
    against it, and `twist_of` is the yaw's general form;
  - `root_motion_over` reads a span of play with any number of wraps, and backwards as the
    forward motion undone (`RootMotion::inverse`);
  - `RootMotion::blend` blends by direction and length (ozz's way), and `angle_about` gives
    the turn.
- **Ticks** (`Assets::move_roots`, after the animators move on and before the sockets are
  placed):
  - each character whose animator takes the motion out reads its clip over the tick's
    span (`previous..time`, unwrapped);
  - through a fade it also reads the clip fading out over its own span, and blends the two
    by the fade's weight;
  - the motion goes into the mesh's space, seen from the clip's first moment's facing
    (the pose keeps that facing), then into the parent's space through the entity's
    `Transform`;
  - `Move` adds it to the translation and turns the rotation about the mesh's up.
- **Frames:** the pose takes the motion out of each sampled clip (`remove_root_motion`)
  before any fade blends them. The root keeps its height and tilt. The sockets read the same
  pose. `Look` (a clip's moment, the fade, the motion taken out or not) now carries what a
  skinned model shows.
- **Tests:**
  - `engine_anim`:
    - spans that wrap twice, and backwards spans that undo forward ones;
    - the blend's direction and length, and the turn read about the up;
    - a root under an armature standing Z up: its ground is x and y, and read with +Y up
      the same clip crosses none;
  - every test rig: up seen from the root's parent is the mesh's up; each clip's motion over
    its whole length crosses as far as its root speed says and stays on the ground; the
    root stays on its spot with the motion taken out;
  - on both GPUs, a walker generated in the test: a box skinned to two joints, `Walk` 1 m a
    second along +z, `Turn` a quarter a second:
    - `Move` crossed 1.25 m in 80 ticks, while every frame's hips stayed within 1 mm of
      the spot;
    - `Report` stayed put and reported 1/64 m each tick;
    - `Turn` turned a quarter a second on the spot;
    - `InPose` walked its loop in place;
  - a script sets `root_motion` by name;
  - 0 allocations with a character's root motion moving it every tick (both builds).
- **E12-2 is done with this:** the plain `Animator` plays a clip, a cross-fade and root
  motion, saved, with `play` for scripts. Under decision 1c the component plays a clip or
  a cross-fade by itself; layers, masks and additive blends are a graph's (E12-5, over
  `engine_anim`'s `Blender`, which blends them already). The animator's other script calls
  need what they read: `set`, `trigger`, `state` and `in_window` a graph's parameters and
  states (E12-5), `on_animation_event` the clips' events (E12-3).
- **Left:**
  - per-clip root motion settings (vertical, no yaw), with the Animation tab's loop
    settings in the model's `.meta` (E12-3);
  - the floor moving under root motion in the editor's previews (E12-1b-3);
  - P in place still plays a clip as made, its root motion in the pose.

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
