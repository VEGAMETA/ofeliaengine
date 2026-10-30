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
