# Animation state machines and blend graphs

Date: 2026-10-29. Input for [ADR 0124](../adr/0124-animation-graphs.md): the graph layer over
`engine_anim` and the player's animator, ahead of E12 ([ADR 0107](../adr/0107-editor.md)).
The code of Godot, Bevy, Fyrox and ozz-animation (MIT; Bevy MIT OR Apache-2.0) was read for
ideas only: **no code was copied**.
Unreal and Unity were studied from their documentation. Nothing was built or run for this
note: numbers are ADR 0068's measurements, or marked as estimates.

## Where we start

- `engine_anim` (ADR 0068) has the steps a graph needs: stateless `CompressedClip::sample`
  (about 0.33 µs per 25-joint clip on one thread), `Blender` (weighted layers, per-joint masks,
  additive layers, rest pose below a threshold), `root_motion` (one wrap), `local_to_model`, IK,
  `for_each_parallel` (1,000 Fox characters, two clips each: 0.13 ms on 16 threads). There is no
  animator: a game blends in `Game::animate` (every rendered frame, after the ticks) and sets
  its `SkinnedMesh` palette.
- Already decided: 64 Hz ticks, rendering interpolated between the last two, live animation
  state never rolled back (ADR 0019); saves keep "animation players" (ADR 0104); scripts and
  saves see reflected components (ADR 0114); `Tween` resolves `Component.field` once to
  (component, offset, lanes) and writes raw floats; graph files use the editor grammar (ADR 0123).

## Sources

| Source | Version / date checked | License | What we looked at |
|---|---|---|---|
| [Godot](https://github.com/godotengine/godot) `scene/animation/` (mixer, tree, state machine, blend tree, blend spaces 1D/2D, player), `scene/resources/animation.*` | 4.7.2-stable, commit `ed1daf0b` (2026-08-17), read 2026-10-29 | MIT | per-node instance data and per-track weights; one flat mixing pass; state machine playback (switch and advance modes, priorities, travel by A*); blend-space triangulation and sync modes; method, value, audio tracks; root motion |
| [Bevy](https://github.com/bevyengine/bevy) `crates/bevy_animation/src/` (`graph.rs`, `lib.rs`, `transition.rs`, `animation_curves.rs`, `animation_event.rs`) | v0.20.1, commit `a0577b5c` (2026-10-28), read 2026-10-29 | MIT OR Apache-2.0 | graph asset (clip/blend/add DAG, mask groups); per-target post-order; blend-stack evaluators; `AnimationTransitions`; events across loops; animated component fields |
| [Fyrox](https://github.com/FyroxEngine/Fyrox) `fyrox-animation/src/` (`lib.rs`, `machine/*`, `pose.rs`, `signal.rs`, `value.rs`) | v1.0.0, commit `a4907aa5` (2026-03-24), read 2026-10-29 | MIT | layered state machine, parameters, logic-node conditions, blend nodes, Delaunay blend space, signals, root motion, event gathering strategies |
| [ozz-animation](https://github.com/guillaumeblanc/ozz-animation) `track_triggering_job`, `motion_blending_job`, `track`; samples `blend`, `partial_blend`, `user_channel`, `motion_blend` | 0.17.0, commit `744eb9d9` (2026-08-01), read 2026-10-29 | MIT | speeds synchronized by a weighted loop length; edge triggering over any time range; root motion blending; float user channels |
| Unreal Engine 5.8 docs: [Sync Groups](https://dev.epicgames.com/documentation/en-us/unreal-engine/animation-sync-groups-in-unreal-engine), [Transition Rules](https://dev.epicgames.com/documentation/en-us/unreal-engine/transition-rules-in-unreal-engine), [Blend Spaces](https://dev.epicgames.com/documentation/en-us/unreal-engine/blend-spaces-in-unreal-engine) | checked 2026-10-29 | proprietary (source **not read**) | leaders, followers, markers; follower notifies suppressed; priority order; inertialization as a blend logic; automatic rule (exit time); input smoothing, weight speed, notify trigger mode |
| Unity 6.4 manual: [Transitions](https://docs.unity3d.com/Manual/class-Transition.html), [Blend Trees](https://docs.unity3d.com/Manual/class-BlendTree.html), [2D Blending](https://docs.unity3d.com/Manual/BlendTree-2DBlending.html); blog "[Wait, I've changed my mind](https://unity.com/blog/engine-platform/state-machine-transition-interruptions)" | checked 2026-10-29 | proprietary | exit time, interruption sources, frozen pose on interruption, normalized time in blend trees, freeform 2D modes |
| D. Bollo, "[Inertialization: High-Performance Animation Transitions in Gears of War](https://gdcvault.com/play/1025165/Inertialization-High-Performance-Animation-Transitions)", GDC 2018 (and his SIGGRAPH 2017 talk) | talk | — | evaluate only the destination; decay the source's offset and velocity with a quintic |
| D. Holden, "[Dead Blending](https://theorangeduck.com/page/dead-blending)" (2023-02-25), "[Spring-It-On](https://theorangeduck.com/page/spring-roll-call)" (2021) | articles, read 2026-10-29 | code license not stated: ideas only | extrapolate the source, then cross-fade; inertialization with critically damped springs |
| R. S. Johansen, *Automated Semi-Procedural Animation for Character Locomotion*, MSc thesis, Aarhus University, 2009, §6.3 | thesis | — | gradient band interpolation (Unity's freeform 2D modes) |

## How they work

### Godot 4.7

- **Asset and instance.** `AnimationNode` resources (shareable) form the tree; an `AnimationTree`
  keeps an `AnimationNodeInstance` per node path: parameters as Variants in a map keyed by
  `parameters/<path>/<name>` (the hottest in fixed slots), the node's weight per track, children.
- **A frame** (idle by default). Each node scales the per-track weights it got by its blend and
  filter and hands them down; a clip leaf appends an instance (clip, time, weights) to the mixer,
  which makes **one flat pass**: per track `rest + Σ wᵢ(vᵢ − rest)` (trees are "deterministic":
  not normalized, so additive is one more weight), rotations by successive
  `slerp(1, rest⁻¹·qᵢ, wᵢ)` (order-dependent). No intermediate poses. Every frame blend spaces
  build `LocalVector`s, key lookups push into vectors, values are Variants; one thread a tree.
- **State machine.** A transition has a switch mode (immediate; sync: the destination starts at
  the source's position *in seconds*; at end: when remaining time ≤ its fade), an advance mode
  (disabled; enabled = only by `travel`; auto), a bool parameter and/or an `Expression` run on a
  base node, a priority (lowest wins), a fade time and curve, reset, break-loop-at-end.
  `travel(state)` plans a path by A* costed by **distances between the states on the editor
  canvas** × priority. During a fade the source keeps playing (live) and **no transition is
  checked**: only a teleport (`start`, a `travel` with no path) cuts it. Zero-time ones chain within a
  frame (loop guard). Nested and grouped machines. Blend-tree nodes: Blend2/3, Add2/3, Sub2,
  OneShot (fire/abort/fade-out), Transition (index switch), TimeScale, TimeSeek, per-node filters.
- **Blend spaces.** 1D: the two neighbours lerped. 2D: Delaunay (automatic) or hand-made
  triangles, barycentric weights, the closest hull edge outside, the triangle found by a linear
  scan. **Sync modes:** none (inactive points frozen), independent (all advance), cyclic-mutable
  (each point advances by its length / the weighted mean length: normalized-time sync),
  cyclic-constant (one cycle per given length).
- **Events and tracks.** Method keys fire in `(previous, current]` (backwards `[ )`), a wrap split
  into two ranges (one wrap per update), for **every** instance with weight > 0 (fades clamp to
  ε, so both sides of a cross-fade fire). Value tracks on any property (continuous, discrete,
  capture), Bezier, blend shapes, audio (polyphonic, volume follows the weight).
- **Root motion:** one track path; per instance the delta over the frame (one wrap) times its
  weight, accumulated; the game reads `get_root_motion_*`. **Format:** `.tres` text
  (`states/<name>/node`, `transitions = [from, to, SubResource(…), …]`); names are the ids.

### Bevy 0.20

- **Asset and instance.** `AnimationGraph` (asset, `.animgraph.ron`): a petgraph DAG of clip,
  blend and add nodes, each with a weight and a 64-bit mask of mask groups; `AnimationPlayer`
  (component): node → `ActiveAnimation` (weight, speed, seek time); bones are entities.
- **A frame** (`PostUpdate`). Players advance in parallel; then every **target** (bone entity) in
  parallel walks its precomputed post-order subgraph, pushes each curve's sample onto a
  per-property evaluator stack (blend: running weighted interpolation; add: additive) and commits
  into its components. Evaluators are boxed trait objects in hash maps, cached per thread.
- **Transitions** (`AnimationTransitions`, "may be replaced by a state machine"): `play(new,
  duration)` pushes the old main animation onto a fading list whose weights decline linearly;
  sources stay live; any `play` interrupts.
- **Properties:** any component field (`animated_field!(Transform::translation)`). **Events:** per
  clip, untargeted or per target, triggered as observers through `Commands` when the seek time
  passes them (`[last, this)`, one wrap per update). No state machine, blend spaces, phase sync
  or root motion.

### Fyrox 1.0

- **Asset and instance in one struct:** a `Machine` (parameters, layers) lives in a scene node
  with its runtime fields. A layer has pools of pose nodes, states and transitions, a weight and
  a mask (excluded bones); clips (own time, signals, root motion settings) live in a player node.
- **A frame** ticks the clips of the active state or transition, then evaluates **every state's
  pose tree** in each layer; poses are hash maps keyed by node handle; parameters are looked up by
  string. With no transition active, the first true one from the active state (pool order)
  starts; it cannot be interrupted; its fade is linear between two live poses.
- **Parameters:** weight, rule, index, sampling point (2D); conditions are and/or/xor/not trees.
  **Nodes:** play, blend, blend by index (a fade per input), 2D blend space (Delaunay from the
  `spade` crate, barycentric, closest edge outside); states run enter/leave actions.
- **Events:** signals fire when the time crosses them, but not across the loop point (a signal
  just after it is missed that update), each cloning its name; state enter/leave events in a
  fixed queue; a blend's events from all inputs, the heaviest or the lightest. **Root motion:**
  per clip, blended with the weights. **Format:** Fyrox's `Visit`, pool handles as ids.

### ozz-animation 0.17 (beyond our first note)

- **Speed sync** (`sample_blend`): the loop length is the weight-averaged length of the active
  clips, each clip's speed its length divided by it: walk and run stay in phase.
- **`TrackTriggeringJob`:** edges of a float track crossing a threshold between any `from` and
  `to`, **several loops and backwards included**, exact crossing ratios, a lazy iterator, no
  allocation. **`MotionBlendingJob`:** root-motion deltas blended with weights, directions and
  lengths apart (two perpendicular walks keep their speed), rotations nlerped.

### Unreal Engine 5 and Unity (documentation), and transitions without a second pose

- **Unreal sync groups:** the highest-weight member leads (or an always/transition leader);
  followers take its length or, with **markers** (e.g. foot down), its position between the
  markers all members share, falling back to length; followers' notifies are suppressed.
- **Unreal transitions:** smallest priority first; blend logic standard, inertialization (a node
  after the state machine; "under about 0.4 seconds") or custom; an automatic rule on the
  player's remaining time (exit time). Blend spaces triangulate, smooth the input (spring damper
  among others), cap how fast weights change, fire notifies from all samples, the heaviest, or none.
- **Unity:** exit time in normalized time (below 1 every loop), duration in seconds or normalized,
  an offset into the destination, interruption by the current and/or next state's transitions,
  Any State first; an **interrupted transition freezes its pose** as the new source ("simply put:
  performance"). Blend trees use normalized time; 2D freeform modes are Johansen's gradient bands.
- **Inertialization** (Bollo): at the switch keep per joint the offset from the destination to the
  source and its velocity, decay it to zero with a quintic; only the destination is evaluated; an
  interruption starts again from the current output. Holden's **dead blending** extrapolates the
  source with a decaying velocity and cross-fades; his springs inertialize with damped springs.

### Side by side

| | Godot 4.7 | Bevy 0.20 | Fyrox 1.0 | Unreal / Unity |
|---|---|---|---|---|
| Evaluation | weights down the tree, one flat mix | per bone entity, a stack per property | every state's tree, hash-map poses | graph per character on workers |
| Fade source | live, not interruptible | live, interruptible (fading list) | live, not interruptible | UE live or inertialized; Unity live, frozen when interrupted |
| Exit time | at end (remaining ≤ fade) | — | "animation ended" | UE automatic rule; Unity normalized exit time |
| Phase sync | blend-space cyclic modes; switch at equal seconds | — | — | UE groups and markers; Unity normalized time |
| Blend spaces | 1D, 2D Delaunay | — | 2D Delaunay | UE triangulation + smoothing; Unity gradient bands |
| Events | one wrap; every weight > 0 | one wrap | none across the loop point | UE: leader only in a sync group; blend space all / heaviest / none |
| Format | `.tres`, names as ids | RON, indices | `Visit`, pool handles | binary (UE), YAML (Unity) |

## What we take (proposed)

### Asset and instance

- **The graph asset** (`.agraph`, text, sketch below) cooks into an immutable
  `engine_anim::AnimGraph`: flat `u16`-indexed arrays (parameters, layers, nodes in post order,
  states, transitions sorted by source and priority, condition bytecode, blend-space triangles,
  sync groups), bound once per skeleton (bone names → joints, masks → per-joint weights).
- **The animator** (`Animator`, a reflected component beside `Model`): the graph (`AssetRef`),
  16 `f32` parameter slots (floats, bools, ints, trigger counts; names in the graph), speed,
  paused. Scripts, the inspector, saves and replication see it as any component.
- **Its runtime state:** one fixed-size block per character laid out by the graph at spawn
  (Godot's per-node instances, flattened): per machine its state, transition and time; per clip
  node its phase now and at the last tick; per sync group its leader and phase; per blend space
  its smoothed input and last triangle; the event ring; inertialization history only if used.
  In `engine_anim`'s pool, named by index from the component (as `SkinnedMesh` names its
  `SkinnedId`), written by the save provider (ADR 0104).

### When it runs: decisions in ticks, poses in frames (question 1)

- **Each tick**, after the scripts' `on_tick` and before `Game::tick`: triggers consumed,
  transitions taken, phases advanced, the tick's events collected, its root motion computed for
  the character controller in `Game::tick`. The same at any frame rate and on every machine;
  saved and replicated with the simulation.
- **Each rendered frame:** the pose at the frame's interpolated instant (`alpha` between the last
  two ticks, as transforms are drawn): active clips sampled at the lerp of their unwrapped phases
  (sampling is stateless), weights lerped alike; then root motion removed, local to model, the
  game's IK (`Game::animate` stays for it), skinning matrices, palette. Hidden characters may skip
  the pose; a headless server or a hit-capsule query evaluates it at the tick itself (alpha 1).

### Evaluation

- The tick computes node weights top down and writes the **active node list** (post order,
  weight above ε); the frame runs only those as ops on a small pose stack: `sample(clip, phase)`,
  `blend(k)` = one `Blender::blend` over k stack slots with weights, masks and modes (chains of
  linear blends fused), `inertialize`, `output`. Stack depth known at cook; scratch poses from
  per-worker arenas. No hash maps, strings, Variants or trait objects on the way.
- Characters in parallel with `for_each_parallel`; events and property writes go to per-chunk
  buffers merged in entity order afterwards, so results never depend on threads.

### Transitions (question 2)

- **Conditions** are expressions over parameters in the file (`speed > 0.1 and grounded`),
  compiled to bytecode at cook (errors marked on the transition, as Slang's on nodes); a trigger
  is consumed by the transition that uses it (Unity). No script runs inside the decision.
- **Choice:** Any State transitions first, then the state's, smallest priority first; at most 4
  chained per tick (loop guard). Exit time in normalized time (covers Godot's "at end"); an
  offset into the destination; "keep phase" starts it at the source's *normalized* phase
  (Godot's sync uses seconds, wrong for clips of other lengths).
- **Blends:** a cross-fade with a live source (curve per transition) or inertialization (only the
  destination evaluated: half the sampling while it lasts). An interrupted transition goes on as
  an inertialization from the current output: no frozen pose (Unity), no growing stack of fading
  sources (Bevy), and interruption allowed (Godot and Fyrox forbid it).
- One-shots (an attack, a hit reaction) are a node: fired and aborted by triggers, fades, a mask
  and a mode (Godot's OneShot, Unreal's slots).

### Phase sync (question 3)

- **Sync groups by name** within a layer, every blend space an implicit one: the heaviest member
  leads, the group's phase advances by `dt / Σ wᵢ·lengthᵢ`, every member samples at `phase × its
  length` (ozz, Godot's cyclic-mutable, Unity's normalized time). Followers' events are
  suppressed unless marked (Unreal). **Markers** later, in the clip's data beside its events.

### Blend spaces and layers

- 1D: sorted points, binary search, lerp. 2D: Delaunay at cook (our own Bowyer–Watson for tens of
  points, not `spade`), barycentric weights, the nearest hull edge outside, the triangle found by
  walking from last tick's: at most 3 clips sampled. Per-axis input smoothing (a critically
  damped spring) and an optional cap on how fast weights move, so a parameter jump does not pop.
  Gradient bands (Unity's polar mode: O(n²), more clips active) later, if artists ask.
- A graph is an ordered list of layers (normal or additive, a constant or parameter weight, an
  optional mask), each rooted at a state machine or a blend tree, combined by one
  `Blender::blend`. Masks are authored as bones with a weight for their subtrees (`spine_01` 1.0,
  `neck` 0.5) and baked per skeleton into per-joint weights (our subtrees are index ranges).

### Events

- Per clip: point events (time, interned id, a small payload) and **windows** (begin, end),
  stored with the clip (for a glTF's clips, beside its import settings), cooked sorted by time.
- Fired over the tick's played interval `(previous, current]`, **any number of loops and
  backwards** (ozz's rule; Godot and Bevy handle one wrap, Fyrox none); seeks and teleports fire
  nothing. By default the leader of a sync group or the heaviest sample of a blend space fires;
  per event `always` or a weight threshold (Unreal's settings, Fyrox's strategies); a source
  stops firing when its transition starts unless marked, so a cross-fade never fires twice.
- Delivered as plain records (entity, event, clip, time, weight) into the animator's
  fixed-capacity ring (overflow counted and warned): Rust reads them after the step, scripts get
  `on_animation_event` in the next tick's script stage, as contacts are delivered today.
- **Windows are state, not events:** `animator:in_window("hit")` holds while the heaviest clip is
  inside one, so gameplay (damage, cancel windows) reads state that replicates and loads, as the
  networking decision asks of anything not one-shot.

### Root motion and property tracks (question 4)

- **Root motion:** per active clip, `root_motion` over the tick's interval (extended to several
  loops), blended as ozz's `MotionBlendingJob` does, summed into the animator's `RootMotion` for
  `Game::tick`'s character controller (or applied to the transform); under inertialization from
  the destination only. The frame removes it from the pose.
- **Property tracks:** a clip may animate number fields of components (1–4 floats: a material
  parameter, a light's intensity, a field of view, morph weights once imported), aimed at
  `child/path:Component.field` from the animator's entity, compressed as joints are. Bound at
  spawn with `Tween`'s resolver (component, offset, lanes: 16 bytes a track), blended by Godot's
  deterministic rule (`bound value + Σ wᵢ(vᵢ − bound)`), sampled and written each tick after the
  parallel step (tweens and lights follow ticks today).
  Discrete values (bools, asset swaps) take the heaviest clip's. Sounds are events played with
  `audio.play_at` on a joint; method tracks become events.

### The file (sketch, the editor grammar of ADR 0123)

```text
[agraph version=1 id=3f1c9a2e7b6d4e10a8c5d2f1e0b9a7c6]

skeleton = asset("fox/Fox.gltf", 6c0d2e4f5a6b4c7d8e9f0a1b2c3d4e5f)
params["speed"].kind = "float"
layers["base"].root = "m0a1"
nodes["m0a1"].kind = "machine"
states["s7c2"].name = "Idle"
states["s7c2"].pose = "c1d9"
states["s3f0"].name = "Move"
states["s3f0"].pose = "b5e4"
nodes["c1d9"].kind = "clip"
nodes["c1d9"].clip = "Survey"
nodes["b5e4"].kind = "blend1d"
nodes["b5e4"].points = [[0.0, "Walk"], [3.5, "Run"]]
transitions["t2b8"].from = "s7c2"
transitions["t2b8"].to = "s3f0"
transitions["t2b8"].when = "speed > 0.1"
transitions["t2b8"].blend = 0.2
```

- States, nodes and transitions keyed by random ids; names are labels (a rename touches no
  reference); conditions are text; sorted on write, so a diff shows what changed.

### Measurements before the ADR

- `anim_bench` grows a locomotion graph (idle; a 2D blend space of 9 clips; an upper-body layer
  with a mask; an additive lean): 100, 1,000 and 10,000 characters on 1 and 16 threads; steady,
  during cross-fades and inertialized transitions; tick and frame parts apart. Zero allocations
  with transitions and events every tick; the same events at the same ticks at 30, 60, 144 and
  240 fps; a save and load in the middle of a transition.
- Estimate from ADR 0068 (to be measured): steady locomotion samples at most 3 clips (about 1 µs a
  character on one thread, plus about 0.4 µs to blend and convert); a cross-fade between two such
  states samples 6 while it lasts; inertialization keeps 3 plus a per-joint decay.

## What we do not take, and why

- **Godot's flat per-track mix:** elegant without intermediate poses, but its rest-relative
  successive slerps depend on order, hash-map track caches and Variants cost on every track, and
  a node that needs a pose (IK, model-space blends) cannot sit in the middle. Expression
  conditions run an interpreter per transition per frame; travel costs from canvas positions let
  the layout change behaviour.
- **Bevy's per-bone evaluation:** a hash lookup and a dynamic evaluator per property per bone
  entity, where our skeletons are flat arrays done in one pass; its RON graph keyed by indices
  shifts when a node is removed (ADR 0123's reason for ids).
- **Fyrox's** evaluation of every state's tree each frame, hash-map poses, string-named
  parameters, signals that miss the loop point and allocate a name each.
- **Frozen poses on interruption (Unity)** and **growing fade stacks (Bevy):** inertialization
  covers interruptions at a fixed cost. **Callbacks and method tracks run from the evaluation**
  (Godot, Bevy's observers): it runs on workers, so events are records read afterwards.
  **Scripted transition rules** (Blueprints, Godot's Expression): compiled expressions cover them.
- **Gradient bands or RBF ("Verbs and Adverbs") as the blend-space default:** more clips active
  per frame (gradient bands cost O(n²) in the samples); RBF can overshoot.
- **Motion matching:** its own decision (patents, ADR 0068's note).

## Patents (checked 2026-10-29: web searches of Google Patents, Justia)

- Inertialization (Bollo), dead blending, normalized-time and marker sync, gradient bands: none
  found; a proper search before inertialization ships.
- RBF interpolation of example motions (Microsoft, US7091975B1): expired 2022-01-12; not planned.
- Motion matching variants: patented (ADR 0068's research note); not part of this work.

## Questions for the team

1. **When the animation decides.** The simulation runs 64 times a second; the screen draws at its
   own rate. An animation *decides* (its state, when a blend starts, clip times, "foot down" or
   "hit" events, how far root motion moves the character) and *draws* a pose.
   - a. Decide in ticks, draw every frame between the last two ticks (as moving objects are drawn
     now). Events and root motion fall at tick times, equal at 60 and 240 fps and on every
     machine, saved and sent over the network with the rest. Costs a small per-tick step.
   - b. All of it every frame (Godot's and Unity's default, Bevy). Simpler, but a "hit" lands at
     other moments at other frame rates, and root motion moves the character outside the
     simulation, which networking and saves cannot follow.
   - Recommendation: a.
2. **How a change of state blends.**
   - a. Cross-fade: both animations play during the blend. Familiar, right for long blends; twice
     the sampling while it lasts.
   - b. Inertialization (Gears of War, Unreal): only the new animation plays; the old pose's
     difference fades out. Half the cost, smooth when the player changes their mind; can look
     wrong on long blends between very different poses (Unreal: under 0.4 s); ~150 bytes a joint.
   - c. Both, per transition: inertialization by default for short blends and always when a blend
     is interrupted; cross-fade where a long blend is wanted. The ~150 bytes a joint (10 KB for
     64 joints) are spent only by graphs that inertialize.
   - Recommendation: c.
3. **Keeping walk and run in step.**
   - a. By fraction of the cycle now: the clips of a blend space or a named group play at the same
     fraction, the heaviest setting the pace (Unity, ozz, Godot). Needs clips whose feet land at
     the same fractions (left at 0, right at 0.5), as most locomotion sets do.
   - b. Markers as well: named marks (left foot down…) in each clip, the others lining up between
     the same marks (Unreal). Handles other step counts (a limp, a start), but needs a marker
     track in E12's timeline (or foot plants detected at import).
   - Recommendation: a now, b with E12's timeline.
4. **What a clip may animate besides bones.**
   - a. Bones and events only.
   - b. Also number fields of components by the `Component.field` paths tweens use (material
     parameters, a light's intensity, field of view, morph weights once imported), blended like
     bones: 16 bytes per track per character, bound at spawn, one write pass a tick.
   - c. b plus on/off and asset switches (visibility, a mesh swap), the heaviest clip winning.
   - Recommendation: b; c when a game needs it.
5. **What scripts see** (the public scripting API is the team's call, CLAUDE.md §3).
   - a. An `Animator` component: `animator:set("speed", v)`, `animator:trigger("jump")`,
     `animator.state`, `animator:in_window("hit")`, and an `on_animation_event(name, data)` hook;
     names looked up once and cached, as tweens do.
   - b. a, plus each graph's parameter names in the generated `.d.luau` (completion, typos
     caught); a generator step per graph in the cook.
   - c. No script access until E12.
   - Recommendation: a now, b with E12.
6. **The file's name.** a. `.agraph` (pairs with `.mgraph`); b. `.animgraph` (Bevy's); c. `.anim`.
   Recommendation: a.
