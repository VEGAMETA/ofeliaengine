# ADR 0124: Animation in scenes, graphs, and the editor's animation tools (E12)

- Status: **proposed** (2026-10-29): the questions at the end go to the team; the steps
  that do not depend on them start now.
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
