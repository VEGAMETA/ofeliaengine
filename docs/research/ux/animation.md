# Animation tools (E12)

Checked: 2026-10-29. Products were studied as products, from public documentation and
forum threads; facts marked * were not found in today's documentation (from use and older
documentation). Godot (MIT) was read for ideas only: at 4.5-stable, how its state machine
editor draws the playing state (`editor/animation/animation_state_machine_editor.cpp`). No
code was carried over. Input for [ADR 0124](../../adr/0124-animation-graphs.md) and
[ADR 0107](../../adr/0107-editor.md) §7, phase E12, after
[ADR 0068](../../adr/0068-skeletal-animation-and-skinning.md) (skeletons, clips, blending,
IK) and [ADR 0123](../../adr/0123-material-graphs.md) (the canvas).

CLAUDE.md §12: everything undoable, immediate feedback, few modes (Raskin), keyboard-first,
errors next to their cause, the editor held to the game's speed. §13: skeletal animation,
blending, state machines, IK, with ozz-animation as the reference. §5: nothing paid where
unused, no allocation per frame. ADR 0107's E12: "clip preview, the skeleton, the timeline
and keys, state machines, blending".

## Where the engine starts

- **`engine_anim`** (ADR 0068): skeletons sorted depth first, so a joint's subtree is a
  range; clips compressed within an error measured where the skin is (0.1 mm by default);
  sampling at any time, in any order (about 0.3 µs a 25-joint pose); `Blender` with
  weighted layers, per-joint masks and additive layers; root motion (horizontal translation
  and yaw by default); two-bone IK, FABRIK, look-at. 1,000 Fox characters take 0.13 ms on
  16 threads, with no allocation.
- **Import and cook:** glTF skins and clips (step, linear, cubic spline; morph weights
  skipped). `cook_scene` compresses every clip that moves a skin into the pack, and the
  compressor reports each clip's bytes and largest error.
- **The player:** `engine_runtime::SkinnedMesh` draws with the palette the game sets in
  `Game::animate`. Nothing plays a clip by itself: no animator component, no state machine,
  no events, no clip in a scene. `Tween` animates one reflected field by its path
  (`"Transform.translation"`), eased and repeating, on the game's clock, and is saved like
  any component.
- **Scripts** (ADR 0114): `e:send("open")` calls the hook `open` on an entity's scripts.
  "Animation control beyond components" was left for a step of its own, so its calls go to
  the owner.
- **The editor:**
  - the bottom bar's Animation tab has nothing behind it;
  - a dropped model becomes an entity per glTF node, a skin's joints included, and its mesh
    draws unskinned, as bound (`engine_app::assets` reads no joints or weights);
  - picking sees skinned meshes in their pose (E2-3); Tab over the viewport edits brushes'
    faces (E2b-5a); P plays the selected sounds where they are, writing nothing into the
    scene (E11-4b-2); while editing, the scene stands still (E3-1);
  - E10's canvas (Space's node search, links dropped on empty canvas, frames, Ctrl+G, Tab)
    was built to be reused by state machines (ADR 0123); previews are drawn by the engine
    (ADR 0112); the dock's Auto Play plays each sound as it is picked (E11-3b).

## How five tools do it

| Tool | What it has | What works | What annoys |
|---|---|---|---|
| Godot 4.7 (AnimationPlayer, AnimationTree, Skeleton3D) | Advanced Import Settings (a double-click on the file) preview a model's animations, slice one into several, and Save to File; the Animation bottom panel (Alt+N): tracks of any node's property, keyed with the key icon beside each inspector field while the panel is open (K in 3D), and position, rotation, scale, blend shape, call method, Bezier, audio and animation tracks; nearest, linear or cubic per track; onion skinning; markers (Shift+click two to play the span between); a RESET animation and Reset On Save; AnimationTree: blend trees (Blend2/3, OneShot, TimeScale, Transition, per-track filters), BlendSpace1D/2D (triangles made as points are added, snap 0.1, sync modes), state machines (switch Immediate, Sync or At End; advance by `travel()`, a condition or an expression; crossfade; priority; machines in machines); `RootMotionView`, a floor for root motion in the editor; BoneAttachment3D | any property keyed in one click from the inspector; one player for bones, fields, sounds and calls; ▶ on each state starts or travels to it in the editor, and the playing state, the one fading out and the travel path are drawn, a line under the name showing progress (its source); an expression tests anything, `false` too | an imported clip is read-only, "This animation belongs to an imported scene, so changes to imported tracks will not be saved" (forum, 2025-10-12), until Save to File; call method tracks "are not executed when the animation is previewed in the editor"; an advance condition tests true only; conditions and `travel()` are two ways to move one machine (forum, 2023-12-17); a big tree "is a bit of a mess, with lots of overlapping transition paths", with no filter (forum, 2024-07-26); bones turn only after "clicking the bone icon in the top bar" (forum, 2026-03-31); previews write into the scene, hence RESET; Bezier tracks cannot blend; the game runs in another process, and the tree's editor shows the edited scene's copy* |
| Unity 6.6 (the model's Animation tab, the Animation window, the Animator) | the import tab: a clip list, start and end dragged on a timeline, Loop Time and Loop Pose with match indicators and loop graphs while an end is dragged, root motion per part (Bake Into Pose, Based Upon), a mask, curves, events, a preview (▶, scrubbing, a speed slider, the pivot and mass centre, any avatar); the Animation window: Dopesheet or Curves, Preview (fields the clip drives tinted blue), Record (every change keyed, the timeline red), Add Property (any animatable field of the object or its children), K keys every animated field and Shift+K the changed ones, `,` `.` step frames and Alt+`,` `.` keys, F and A frame; events as markers naming a method; the Animator: states, transitions (Has Exit Time, Exit Time, Fixed Duration, Transition Offset, five interruption sources), parameters, Any State, sub-state machines, layers (Override or Additive, a mask, synced), blend trees (1D, three kinds of 2D, Direct) with Compute Thresholds and Compute Positions from root motion and a red marker to drag | a clip seen where it is imported; the loop seam judged while the cut is dragged; thresholds placed from the clips' own speeds; a transition's overlap dragged on a timeline that shows the feet's contacts; masks made by clicking body parts; in Play, the selected object's state lit with its progress* | Record is a mode; in Preview an edit not keyed is lost when the playhead moves ("you will lose the modification"); Create silently "Creates a new Animator Controller Asset" and adds the component; bones show in the Scene view only through the Animation Rigging package's Bone Renderer; an event names a method on "any script attached to the GameObject", with one parameter; transitions take "hours … through trial and error" to debug (forum, 2017-08-08); a plugin's vendor lists "magic strings" and "impossible to debug them effectively" among Mecanim's six problems (Animancer 8.4) |
| Unreal Engine 5.8 (the animation editors, Animation Blueprints, blend spaces, notifies) | an editor with its own viewport per asset kind (sequence, blend space, montage, skeleton, Blueprint) and an Asset Browser filtered to the skeleton (a hover shows a preview, a double-click plays it); a Rate Scale; root motion previewed Ignore, Loop or Loop and Reset, its trajectory and facing drawn; the Skeleton Tree (filters, sockets, virtual bones, blend masks); notifies on tracks, Sound and Niagara built in, notify states with a length, a trigger weight threshold, sync markers, a sync group's followers firing none by default; FootstepAnimEventsModifier marks foot plants with notifies or sync markers; blend spaces: axes with divisions and snap, animations dropped on the grid, Ctrl moves a crosshair to preview and Ctrl+Alt shows the weights, thin triangles flagged red, smoothing, analysis that places samples; state machines: a rule graph per transition, an automatic rule near a clip's end, priority order, Standard, Inertialization or Custom blends, conduits, aliases (a Global Alias), shared rules; the Rewind Debugger: recorded play scrubbed with the Blueprint showing the recorded state | typed events with sound and effects built in; one event per step in a blend (the leader's); foot plants found for you; weights read under the cursor; root motion drawn; recorded play scrubbed with its graph | logic spread over rule graphs opened one by one; Bidirectional: "This setting is unsupported and currently does not function"; skeleton notifies are saved on the Skeleton; inertialization needs a node after the machine; the Rewind Debugger needs a plugin and a restart, and recordings are "stored indefinitely"; one machine "is starting to become too cumbersome" (forum, 2017-12-07); "animation is the bane of my existence" (forum, 2026-02-16); a door is a Blueprint Timeline (a curve, Lerp, SetRelativeRotation), not a clip |
| Blender 5.2 (Dope Sheet, Graph Editor, NLA, armatures) | bones drawn Octahedral, Stick, B-Bone, Envelope or Wire, In Front, with names and axes; Pose mode; the Dope Sheet (modes: Dope Sheet, Action Editor, Shape Key, Grease Pencil, Mask), handle kinds as key shapes, held keys as bars, keyframe types (R), Slide (Shift+T), columns (K, Ctrl+K), keys before and after the frame ([, ]); the Graph Editor; the NLA; auto-keying; motion paths; slotted actions (4.4) | keys read per channel and as a summary; a column of keys taken at a frame with one key; the skeleton seen through the mesh | a clip's life spans three editors; which actions reach a glTF file depends on Stash or Push Down (the manual's 2021 note; four actions exported as three, forum 2021-01-14); after slotted actions, a user saw slots change on every action and exports with empty frames (forum, 2025-04-14; a developer could not reproduce it); onion skins are Grease Pencil's, 3D ghosts come from add-ons |
| Figma (interactive components, easing) | variants as states; "Change to" moves an instance to another variant on a trigger (On click, On hover…), each instance remembering its state; easing presets (Gentle, Quick, Bouncy, Slow) beside custom Bezier and spring curves, drawn as they are picked | a state is what it looks like; a transition reads as a trigger and a motion; a curve seen before it is chosen | interactions are tried in Presentation view, not on the canvas where they are made |

## What we take

1. **Clips are parts of their model** (`hero.glb#Run`), named as meshes and materials are
   (ADR 0108) and found with `#clip:`. Nothing is extracted at import (Unreal's asset per
   clip) or before editing (Godot's Save to File).
2. **Previews where clips are found** (Unity's import tab, E11's sounds): a skinned model
   picked in the dock, or a character selected, shows its Clips: a row each (length,
   frames, root motion speed, bytes, measured error, loop seam) under the engine's preview
   (ADR 0112): ▶, scrubbing by frames, 0.25–2×, loop, turned by dragging. ↑ and ↓ move
   through the clips, and Auto Play plays each one. Ctrl+click a second clip: both play side
   by side in step, to compare takes or check that walk and run plant the same foot first.
3. **Root motion and loop seams seen:** the floor moves under the model (Godot's
   `RootMotionView`), the root's path and facing are drawn (Unreal's), In Place holds it
   still. A loop's seam is a number (Unity's match lights): how far the skin jumps from the
   last frame to the first, by ADR 0068's virtual vertices with root motion taken out
   ("loops within 0.2 mm", or "⚠ jumps 3.1 cm at Foot.L", a click picking that joint).
4. **P plays in place** (E11's key): the selected characters play their clip or graph
   where they stand, until P or Escape. Nothing is written into the scene, so no RESET
   animation is needed (Godot). While editing, a character stands in its first state's
   first frame, so a seated guard looks seated while the level is laid out.
5. **Joints are not entities** (question 2): a skinned file's drop makes one entity for the
   skin. The inspector lists its joints in a tree with a filter (Godot's bone tree,
   Unreal's Skeleton Tree).
6. **Joints in the viewport:** the selected character's joints as thin sticks in front of
   the mesh (Blender's Stick, In Front), every character's with View: Skeletons. Tab over
   the viewport enters the joints, as it enters brushes' faces: a click picks one, the
   gizmo turns it, the inspector shows it. Unity needs a package for this, Godot a toggle.
7. **Attachments are entities:** an entity's row dropped on a joint, in the viewport or the
   joint tree, follows that joint from where it is (`JointAttachment`, Godot's
   BoneAttachment3D), in one undo step. The character's prefab carries its attachments, as
   Unreal's skeleton sockets would.
8. **The timeline in the Animation tab** (Alt+N, Godot's key), under the viewport: the
   selected entity's clip; frames, with seconds beside; events on top, a summary, the joints
   folded as their tree, then the fields the clip animates. Dope Sheet by default, Curves
   for the selected tracks (Unity's switch). Its keys while it has focus: Space plays, `,`
   and `.` step a frame and Alt with them a key, Home and End, F frames the selected keys.
9. **Pops found, not hunted:** a picked joint leaves a trail (a dot a frame) and its speed
   draws as a curve under the tracks; a spike is a pop, and a click on it moves the
   playhead there. The clip's error per joint says whether the source or the compression
   made it.
10. **Events on every clip** (question 3): E adds a named event at the playhead; a Sound
    event takes a sound field (waveform, ▶, `#sound:`); drag to move, Delete to remove. An
    event calls the hook of its name on the entity's scripts, as `e:send` does (ADR 0114),
    and reaches Rust as an event. Sounds play in every preview (Godot's call tracks do not).
    In a blend only the clip with most weight fires (Unreal's sync-group leader), so walk
    and run do not double the steps.
11. **Foot plants marked for you:** Animation: Mark Foot Plants, with the feet's joints
    picked, puts an event where each foot is lowest and slowest, on every selected clip
    (Unreal's FootstepAnimEventsModifier).
12. **Any field can be keyed** (Godot's tracks, Unity's Add Property): a track names an
    entity at or below the animated one and a reflected field, in `Tween`'s paths
    (`Transform.rotation`, `Lamp:PointLight.intensity`). One clip may hold joints and
    fields: a door, a flickering lamp, a material's parameter.
13. **Keys without a record mode** (question 4): K keys the selection's changed fields at
    the playhead (Unity's Shift+K), and ◆ beside a field keys that one (Godot's key icons);
    a field the shown clip animates keys itself when edited, so no edit is lost (Unity's
    pink fields) and nothing is keyed by accident (its Record). Keys drag in whole frames
    and Shift drags one freely (the owner's rule): a key off the grid makes a clip larger
    (Fox's Run, keyed 0.8 frame off its 24 Hz grid, is sampled at 120 Hz, ADR 0068).
14. **State machines on E10's canvas,** a graph file in a tab of its own (ADR 0123's rule),
    its character previewed in the corner where the material preview is. States are clips,
    blends or machines: Space adds one, a clip dropped is one, a drag from a state's edge is
    a transition, and one dropped on empty canvas makes the next state. Ctrl+G makes a
    machine of the selected states, Tab enters it (E10's keys). An arrow shows its
    condition, an expression over the graph's inputs (`speed > 0.1 and not crouch`, as
    Godot's expressions), or "at end", an exit time, a crossfade; a misspelt input is
    underlined as it is typed (against Unity's "magic strings"). Any holds transitions from
    every state (Unity's Any State). ▶ on a state travels there on the preview (Godot).
15. **Inputs from motion:** an input can follow the entity's movement (speed, its velocity
    in its own frame, turning), so an NPC moved by navigation walks and runs with no code.
    A character moved by its own root motion takes its inputs from the game, since its
    motion follows the blend.
16. **Blends in the inspector** (Unity's blend trees): a blend shows its clips as points on
    a line (1D) or a plane (2D, Delaunay triangles as in Godot and Unreal) and a marker to
    drag, the preview following. Place by Speed or by Velocity puts each clip where its root
    motion is (Unity's Compute Thresholds, Unreal's analysis); a clip made in place gets its
    place typed. A blend's clips keep one phase (Unity's), so the feet stay in step.
17. **Layers and masks:** each layer of a graph has a weight, a mask, and normal or
    additive mode (our `Blender`'s). A mask is made in the joint mode: a click takes a joint
    and those below it, Shift+click the joint alone, tinted on the character (Unity's body
    diagram and transform checkboxes).
18. **Live in Play:** the tree and the inspector show the running world (E3). The selected
    character's Animator section says its state ("Run · 0.42 s · → Jump 30 %"); its graph's
    tab lights the state with a line for its progress and draws the crossfade (Godot's
    drawing), with the inputs' live values in the inspector, where a typed value holds for
    testing. Ejected (Shift+F8), View: Animation States labels every character in the
    viewport. Unity needs its window open and the object selected*, Unreal a debug object*;
    Godot shows the editor's copy*.
19. **As everywhere:** every edit one undo step in the one history; clips and graphs
    autosaved and written by Ctrl+S; their edits in Play apply at once and stay, as the
    mixer's do (E11-1b-3). A character that does not animate costs nothing; graphs run on
    the job system over ADR 0068's steps, with no allocation (measured when built).

## What we do not take

- **A record mode** (Unity's Record, Blender's auto-keying): question 4.
- **Joints as objects** (Unity): question 2.
- **An asset per clip** (Unreal) **or Save to File before editing** (Godot): question 3.
- **A RESET animation** (Godot): previews never write the scene.
- **A rule graph per transition** (Unreal) **and five interruption sources** (Unity): an
  expression on the arrow, and a script decides what an expression cannot.
- **Events naming a method with one argument** (Unity): events are hook names.
- **A second kind of animation for objects** (Unreal's Timelines and Sequencer beside its
  sequences): one clip kind for joints and fields.
- **An editor window per asset** (Unreal): clips preview in the inspector and in place;
  graphs open in tabs.
- **Ghosts of the whole mesh** (Godot's onion skin) at first: trails find pops for less.
  Ghosts can follow, each a skinned instance (28 bytes a vertex, ADR 0068).
- **Retargeting, humanoid avatars, motion matching, control rigs, NLA strips:** no game
  asks for them yet, and each needs its own research.

## The proposal

Illustrative: the inspector's Clips, the Animation tab, a graph's tab.

```text
Clips · hero.glb       [filter      ]                                    Auto Play ☑
  ▶ Idle   2.0 s · 60 frames · in place      4.1 kB · ≤ 0.1 mm · loops within 0.1 mm
    Walk   1.0 s · 30 frames · 1.5 m/s       3.0 kB · ≤ 0.1 mm · loops within 0.3 mm
    Run    0.7 s · 21 frames · 4.2 m/s       2.6 kB · ≤ 0.1 mm · ⚠ jumps 3.1 cm at Foot.L
  ┌─ the model drawn by the engine, its floor moving, its root's path ───────────────┐
  └──────────────────────────────────────────────────────────────────────────────────┘
  ▶ ■ ⟲  ━━━━━━━━●━━━━━━━━  18 / 30 · 0.60 s    1× ▾    In Place ☐

┌ Animation  Guard · Walk ▾   ▶ ■ ⟲   18 / 30 · 0.60 s   1× ▾   K  E   Dope Sheet | Curves ┐
│                      0    5    10   15   20   25   30                                    │
│ Events                  ◆ step         ◆ step                                            │
│ ▾ Guard              ▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 54 joints, sampled at 30 Hz         │
│     Foot.L speed     ▁▁▂▃▂▁▁▁▁▁▁▁▁▁▁▁▁█▁▁▁▁▁▁▁▁▁▁▁▁▁   frame 17: 3.1 cm in one frame     │
│ Lantern: intensity   ◆──────────────◆──────────────◆                                     │
└──────────────────────────────────────────────────────────────────────────────────────────┘

┌ guard.agraph   inputs: speed 3.2 · alert false · attack ⚡            preview: Guard ◯ ┐
│  ┌ Locomotion ▶ ───────┐  speed > 0.1 and alert          ┌ Alert ▶ ─┐                 │
│  │ blend by speed      │ ──────────────────────────────▶ │          │                 │
│  │ ━━━━━━━━━━━━ 62 %   │ ◀─────────── at end ─────────── └──────────┘                 │
│  └─────────────────────┘          Any ── attack ──▶ [ Attack ▶ ]                      │
└───────────────────────────────────────────────────────────────────────────────────────┘
```

| The developer wants to… | Ours | Actions: ours · Unity · Godot · Unreal |
|---|---|---|
| preview a clip of a model just imported | click the model in the dock: its Clips and the preview, the first clip playing (Auto Play); ↓ for the next | 1 · 4 · 3 · 1 after its import dialog* |
| make a character go idle → walk → run by speed | select it; click Idle, Ctrl+click Walk and Run in its Clips; Animation: Blend by Speed: one state blending the three at their root motion speeds, `speed` following its movement | 5 · about 20 and a line of code · about 19 and a line · about 25 |
| add a footstep at frame 12 of Walk | with Walk in the Animation tab: type 12 in the frame box, E, "footstep", Enter; or Mark Foot Plants for every step of every selected clip | 4 · 8 and a method · 12 and a method · 7 |
| find the frame where a foot pops | Tab over the viewport, click Foot.L: its trail and speed curve; click the spike: the playhead is on the pop, and the error row says whether compression made it | 3 · about 9 (no bones in the preview, curves read-only) · 3, by eye · about 7, by eye |
| open a door in one second | select it; Alt+N; New Clip "open"; K at frame 0; click at 1 s; R, Y, 90, Enter (the rotation is animated now, so it keys itself); a script plays it (question 1) | 6 and a line · about 23 and a line · 12 and a line · about 20 (a Blueprint Timeline) |
| see which state a running guard is in | in Play, Ctrl+P "guard": the inspector says "Run · 0.42 s", its graph's tab lights the state; ejected, View: Animation States labels every character | 1 · 4, or 1 with the window open* · about 5 through the remote tree* · 4 through a debug object* |
| put a sword in the right hand | Tab over the viewport; drag the sword's row onto Hand.R | 2 · 1 (bones are objects) · 6 (a BoneAttachment3D) · about 5 (a socket, attached by name) |

Counted from each tool's documented steps, a typed name or value as one action.

### E12's steps

| Step | What | Why there |
|---|---|---|
| **E12-1. Characters in scenes, clips seen** | a skinned file's drop as one entity with its skeleton (question 2), drawn skinned in the editor; the Clips section and its preview with root motion, loop seams and clips side by side; P in place; joints in the viewport, Tab's joint mode, the joint tree; attachments | everything later stands on a character in a scene, and the steps exist (ADR 0068) |
| **E12-2. Clips played in the game** | the `Animator` component (question 1): a clip, a crossfade, layers, from scripts and Rust; root motion to the entity; its state saved like a `Tween`'s | a door and a simple NPC need no more; its calls go to the owner |
| **E12-3. The timeline and events** | the Animation tab: clips shown, scrubbed and stepped; trails and speed curves; events with sounds, kept where question 3 says; Mark Foot Plants; loop and root motion settings | it reads clips that exist; events are what imported clips lack |
| **E12-4. Keys** | tracks of fields; K and ◆ (question 4); the dope sheet's selection, box, move, copy and paste, delete; curves with step, linear and smooth keys; `.anim` files | the door; it stands on E12-3's panel |
| **E12-5. Graphs and blends** | `.agraph` files on E10's canvas: states, transitions with expressions, exit times and crossfades, Any, machines in machines; inputs, from motion too; 1D and 2D blends in the inspector with Place by Speed; layers and masks; the preview and ▶ per state; the live state in Play | the largest interface; it stands on E12-2's component and E10's canvas |
| **E12-6. Later** | the last seconds of play kept (question 5); ghosts; IK in graphs (feet on the ground, look-at: the solvers exist); morph targets (not imported yet); retargeting | each makes the first five better, and none blocks them |

Measured as they land: the preview's frame cost on both GPUs; a graph's cost per character
beside ADR 0068's 0.13 ms for 1,000 Foxes; trails and speed curves of a 200-joint clip; the
Animation tab and a 50-state canvas within ADR 0107's 1 ms of UI; no allocation with the
panels open in Play.

## Questions for the team

Questions 1 to 3 touch formats, the scene model and the scripting API (CLAUDE.md §3).

1. **What makes a character animate in a scene.** Today the game poses skins itself in
   `Game::animate`, and nothing in a scene plays a clip.
   - a. **An `Animator` component naming a graph file** (`.agraph`: states, transitions,
     blends, layers, inputs), run by the player on the job system over ADR 0068's steps.
     Scripts and Rust set its inputs and read its state as fields; reflected components
     reach scripts with no work (ADR 0114). Cost: a file kind, its runner and the canvas;
     its state saved and rolled back like a `Tween`'s; inputs named by strings, which the
     editor checks.
   - b. **Clips played from code only** (Animancer's way in Unity, `animancer.Play("Jump")`):
     the component plays, crossfades and layers clips, and there is no graph file. Cost: the
     least now; every transition is code and is seen only by running it; no state machine,
     which E12's list names.
   - c. **Both:** the component plays a clip, a crossfade or a graph. A door's script plays
     "open" with no graph; a character has one. Cost: a's, plus a few calls (`play`,
     `crossfade`, `set`) that are public scripting API (ADR 0114 left "animation control
     beyond components" for later), so they go to the owner.
   - Recommendation: c, in two steps: the component and its calls (E12-2), the graph
     (E12-5).
2. **Whether a skeleton's joints are entities.** A dropped model makes an entity per glTF
   node today, so a character of 60 joints arrives as 61 entities, 60 of them doing nothing.
   - a. **Not entities** (Godot, Unreal): one entity holds the skeleton; joints show in the
     inspector's tree and in the viewport's joint mode; attachments are child entities
     naming a joint; scripts reach a joint through the component. Cost: the joint mode, the
     tree, `JointAttachment`; scenes saved with joint entities keep them, and drops from
     then on differ.
   - b. **Entities** (Unity): each joint a child with a `Transform`, written by the
     animation every frame; scripts move joints like anything else. Cost: 25 to 200 rows
     per character in the tree, and for 1,000 characters of 60 joints, 60,000 transforms
     written and propagated a frame beside ADR 0068's arrays; Unity has Optimize Game
     Objects to take them away again*.
   - c. **a, with the joints listed under the model in the tree** as rows that are not
     entities. Cost: a second kind of row in the tree.
   - Recommendation: a; c if people look for joints in the tree.
3. **Where the editor's animation data lives.** Imported clips sit inside model files; their
   loop flags, root motion settings and events have nowhere to go, and neither have clips
   made in the editor.
   - a. **An imported clip's settings and events in its model's `.meta`** (Unity keeps
     them in its import settings), so a new export from Blender keeps them. Clips made in
     the editor are `.anim` files and graphs `.agraph` files, in our grammar with their ids
     (ADR 0108). Cost: a section in `.meta`, two file kinds and their cook; an event past
     the end of a shortened clip is kept and marked.
   - b. **Every clip extracted to an `.anim` file at import** (Unreal's asset per clip).
     Cost: the data twice; each new export merged into the files; a model and its clips
     drifting apart.
   - c. **Imported clips read-only, and Extract to edit one** (Godot's Save to File). Cost:
     the step Godot's users trip over; an extracted clip stops following its model.
   - Recommendation: a.
4. **How keys are made.**
   - a. **No record mode:** K keys the selection's changed fields at the playhead, ◆ keys
     one field, and a field the shown clip animates keys itself when changed. Cost: two
     rules (new fields by K, animated ones by themselves), shown by the inspector's marks.
   - b. **A record switch** (Unity's Record, Blender's auto-keying): while it is on, every
     change is a key. Cost: a mode (CLAUDE.md §12); keys made by accident while fixing the
     scene.
   - c. **Only K and ◆** (Godot): an unkeyed change to an animated field is lost when the
     playhead moves, as Unity's Preview warns, or saved into the scene. Cost: lost edits.
   - Recommendation: a.
5. **The last seconds of play, kept.** "Why did he not jump?" is asked after the moment.
   - a. **Live only:** the graph shows the state now. Cost: nothing; a missed transition is
     found by playing again.
   - b. **The last 10 s of each animated character kept while the editor plays:** each
     tick's state, its time, the transition and the inputs, about 48 bytes (30 kB a
     character at 64 ticks a second, 3 MB for 100). Paused (F7), a bar over the graph
     scrubs through them, the canvas and the inspector showing what was (Unreal's Rewind
     Debugger, without a plugin or files). Cost: the ring and the bar; the viewport keeps
     the paused frame.
   - c. **b, and the poses too,** so the viewport shows the past. Cost: about 1.5 MB a
     character for 10 s of 60 joints, and a second way to draw the world.
   - Recommendation: b, in E12-6.

## Sources (checked 2026-10-29)

- Godot 4.7, https://docs.godotengine.org/en/stable/ + `tutorials/animation/introduction`,
  `tutorials/animation/animation_track_types`, `tutorials/animation/animation_tree`,
  `tutorials/assets_pipeline/importing_3d_scenes/advanced_import_settings`,
  `classes/class_animationnodeblendspace2d`; keys (Alt+N, K),
  https://docs.godotengine.org/en/4.3/tutorials/editor/default_key_mapping.html; ideas only,
  https://github.com/godotengine/godot/blob/4.5-stable/editor/animation/animation_state_machine_editor.cpp;
  forums, https://forum.godotengine.org/ + `t/how-to-add-tracks-to-imported-file/124884`
  (2025-10-12, 4.5), `t/animationtree-conditions-or-travel/38014` (2023-12-17, 4.2.1),
  `t/how-much-do-you-need-to-worry-about-creating-spaghetti-when-working-with-the-animation-tree/74108`
  (2024-07-26, 4.2.2), `t/pivot-of-the-root-always-active-even-if-i-select-different-skeleton-bones/136376`
  (2026-03-30 and 31, 4.6.1).
- Unity 6.6, https://docs.unity.com/en-us/engine/current/manual/ +
  `assets-and-media/asset-types/models/importing/class-animation-clip`;
  `animation-section/animation-mecanim/animation-clips/animation-editor-guide/` +
  `animeditor-using-animation-editor`, `animeditor-animating-agame-object`,
  `animeditor-creating-anew-animation-clip`, `script-animation-window-event`;
  `animation-section/animation-mecanim/animation-animator-controller/` + `animator-window`,
  `animation-state-machines/class-transition`, `animation-state-machines/class-state`,
  `animation-state-machines/animation-layers`,
  `animation-state-machines/animation-blend-trees/blend-tree-1dblending`,
  `…/blend-tree-2dblending`; `animation-section/animation-mecanim/avatar-creationand-setup/class-avatar-mask`;
  the loop indicators' colours, https://docs.unity3d.com/2020.2/Documentation/Manual/LoopingAnimationClips.html;
  https://docs.unity3d.com/Packages/com.unity.animation.rigging@1.0/manual/AnimationRiggingMenu.html;
  https://discussions.unity.com/t/debugging-animation-transitions-especially-interruption-failures/673502
  (2017-08-08); Animancer 8.4 (a vendor's comparison), https://kybernetik.com.au/animancer/docs/introduction/
  + `mecanim-vs-animancer/why`, `features`.
- Unreal Engine 5.8, https://dev.epicgames.com/documentation/en-us/unreal-engine/ +
  `animation-sequence-editor-in-unreal-engine`, `animation-notifies-in-unreal-engine`,
  `state-machines-in-unreal-engine`, `transition-rules-in-unreal-engine`,
  `blend-spaces-in-unreal-engine`, `skeleton-editor-in-unreal-engine`,
  `animation-rewind-debugger-in-unreal-engine`, `animation-modifiers-in-unreal-engine`,
  `timelines-in-unreal-engine`, `python-api/class/FootstepAnimEventsModifier`;
  https://dev.epicgames.com/documentation/unreal-engine/API/Editor/UnrealEd/EProcessRootMotionMode
  and `…/EVisualizeRootMotionMode`; the Asset Browser (4.26, 4.27),
  https://docs.unrealengine.com/4.26/en-US/AnimatingObjects/SkeletalMeshAnimation/Persona/AssetBrowser,
  https://docs.unrealengine.com/4.27/AnimatingObjects/SkeletalMeshAnimation/AnimHowTo/PreviewingAnimations;
  forums, https://forums.unrealengine.com/ + `t/multiple-state-machine-questions/103576`
  (2017-12-07), `t/i-have-a-really-hard-time-with-animations-and-animbp-for-game-design/2699996`
  (2026-02-16).
- Blender 5.2 LTS, https://docs.blender.org/manual/en/latest/ +
  `editors/dope_sheet/introduction.html`, `editors/dope_sheet/editing.html`,
  `animation/armatures/properties/display.html`; slotted actions (4.4),
  https://devtalk.blender.org/t/blender-4-4-slotted-actions-feedback/38906,
  https://blenderartists.org/t/the-new-animation-system-problem/1588934 (2025-04-14); actions
  in glTF, https://lists.blender.org/pipermail/bf-docboard-svn/2021-June/003077.html (the
  manual), https://hub.jmonkeyengine.org/t/solved-glb-animation-export/44164 (2021-01-14,
  Blender 2.9); onion skins as an add-on, https://extensions.blender.org/add-ons/b-onion-skin/.
- Figma, https://help.figma.com/hc/en-us/articles/360061175334-Create-interactive-components-with-variants,
  https://help.figma.com/hc/en-us/articles/360051748654-Prototype-easing-and-spring-animations.
