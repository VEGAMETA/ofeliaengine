# Animation: skeletons, clips, blending, IK and skinned meshes

Characters are animated in three steps each frame:

1. **On the CPU** (`engine_anim`): sample clips into poses, blend them, take out root
   motion, convert to model space, apply IK, compute the skinning matrices. Many
   characters run in parallel on the job system, with no allocation per frame.
2. **Hand the matrices to the renderer** (`engine_render::skinning`).
3. **On the GPU**: a compute pass skins every posed instance into its own mesh in the
   geometry pool. Depth prepass, shadows and forward shading then draw it like any mesh;
   motion vectors get a small extra pass.

The design and the measurements are in ADR 0068.

## Skinned models in scenes

A scene's `Model` whose mesh a skinned node of its glTF file draws is drawn skinned by the
player, with nothing to write (ADR 0124's E12-1a):

- The player makes the skin in the background when the model is first drawn. That means
  the mesh's vertices with their joints and weights, and the skin's rig: its skeleton, its
  rest pose, and the file's clips that move it, compressed. The model then draws in its
  rest pose. CesiumMan's took 36–41 ms.
- `Player::model_rig(entity)` gives the rig: `rig.clips()` lists each clip's name, length,
  samples, bytes and largest compression error. It is `None` for a model drawn without a
  skin.
- `Player::pose_model(entity, Some((clip, seconds)))` holds the clip at that moment, held
  at its ends; `None` lets it go, back to its `Animator`'s pose (its rest pose without
  one). Nothing in the scene changes. This is what the editor uses to show a clip in
  place. Posing anew every frame allocates nothing.
- `Player::model_joints(entity, &mut joints)` gives the joints where the pose puts them,
  in the entity's space and the skeleton's order (`rig.skeleton()` names them and their
  parents).
- The skinned vertices land where the plain mesh would: the node's place in the file and
  the file's import scale are taken into account, so the entity's transform places the
  character.
- With material slots (`MaterialSlots`), each primitive is a skin of its own, drawn by the
  model's derived parts, all posed together.
- A file read again makes its skins again; the model is not drawn until the new skin is
  made.

### The `Animator`

An `Animator` on a skinned model's entity plays one of its clips, looped (ADR 0124's
E12-2a):

```rust
use engine_app::prelude::*;

world.spawn((Model::new(mesh, material), Animator::new("Walk")));
```

- `clip` names the clip as its file does. An unnamed clip goes by the name the editor
  shows it by (`Clip 2`). Empty, or a name the file lacks, plays the first.
- `time` is where the clip is, in seconds. Each tick moves it on by the tick's length times
  `speed` (1 by default; negative plays backwards), and it wraps at the clip's end.
  `paused` holds it.
- Each rendered frame poses the character between its last two ticks, as transforms are
  drawn, so a 64 Hz clock still moves smoothly at any frame rate.
- `time` is saved with the entity: a load goes on from there, and setting it moves the
  clip there.
- While the world stands still (the editor editing, the game paused), the character
  stands at `time`: its clip's first frame unless set.
- A pose the editor holds (`Player::pose_model`) wins until it lets go.
- **Fading into another clip** (E12-2b): `animator.play("Run", 0.25)` in Rust, or
  `self.Animator:play("Run", 0.25)` in a script, starts `Run` from its beginning and
  fades from what played over a quarter of a second. Without a fade (or with 0), it
  switches at once.
  - Both clips move on during the fade, at the animator's speed. Each frame blends the
    two poses by how far the fade has gone, between the last two ticks as the clip is.
  - A `play` during a fade drops the clip that was fading out. The clip that was fading
    in fades out from where it is.
  - The fade is saved with the entity, so a load goes on fading. The fields are hidden in
    the editor.
- **Root motion** (E12-2c): a clip that crosses the ground (a walk made moving forward,
  not on a treadmill) can move the character instead of its pose. `root_motion` says how:
  - `InPose` (the default): the motion stays in the pose, as the clip was made. The
    character walks off its spot and jumps back each loop.
  - `Move`: each tick the clip's ground motion over the tick moves the entity's
    `Transform`, and its turn turns it. The pose keeps the root on the spot (its height and
    tilt stay in the pose unless the clip says `root_up`, below), so the character goes
    exactly as far as the clip says.
  - `Report`: the same motion is taken out of the pose, but the entity stays put. Each tick
    the animator's `moved` (in the entity's parent's space) and `turned` (radians about the
    character's up) hold it, for your character controller to move a capsule by.
  - The root is the skin's topmost joint. The ground is the file's: across and about
    glTF's +Y in its scene, wherever its armature or mesh nodes are turned.
  - Through a fade the two clips' motions blend by the fade's weight. A pose the editor
    holds moves nothing, and nothing moves while the world stands still.
  - The editor's Clips show each clip's root speed (E12-1b): near 0 for a clip made in
    place, which has no motion to take out.
- **How each clip plays** lives in its model's `.meta` too (E12-3e), beside its events:
  - `once = true`: the clip plays once and holds its last moment (a death, a door
    opening); the animator's `time` stops at the clip's end. Without it, the clip loops.
  - `root_across`, `root_up`, `root_turn`: what of the root's motion root motion takes out
    of the pose and moves the character by. The default is the ground's two: across and
    the turn. `root_up` takes a jump's rise and fall as well (`moved` gets its height);
    `root_turn = false` leaves a turn in the pose.
  - The Animation tab's header sets them ("in the game: Once · root motion: Across Up
    Turn"), each an undo step written to the `.meta`.
- Scripts reach it as any component: `self.Animator.speed = 2`,
  `self.Animator.root_motion = "Move"`, and `self.Animator.moved` after a tick. The rest
  of its calls (`set`, `trigger`, `state`) come with the rest of E12-2.

In the editor its `clip` is a list of the character's clips.

### Clip events: footsteps and other moments

A clip's events are moments its model's `.meta` names (ADR 0124's E12-3b): a footstep at
0.25 s, a sword's swing at 0.6 s. The `.meta` keeps them beside the file, so a new export
from Blender keeps them too:

```
model.clips.0.name = "Walk"
model.clips.0.events.0.time = 0.25
model.clips.0.events.0.name = "step"
model.clips.0.events.0.data = "left"
model.clips.0.events.0.sound = "sounds/step.wav"
model.clips.0.events.1.time = 0.75
model.clips.0.events.1.name = "step"
model.clips.0.events.1.data = "right"
```

- **A clip by its name in the file.** An unnamed one goes by the name the editor shows it
  by (`Clip 2`). `data` is optional text for whoever hears the event; `sound` is optional
  too.
- **Made in the editor:** the Animation tab's strip adds, moves, names and removes them,
  writing the `.meta` with undo ([editor guide](editor.md)); the lines above are what it
  writes. Its Mark Foot Plants puts `footstep` events (`left`, `right`) where the feet
  touch down.
- **Fired in the ticks.** Each tick, once the animators move on, every character fires the
  events its clip passed over the tick. That holds at any speed: backwards (the latest
  first), and several loops in one tick if the clip is short and fast. An event at a
  clip's very start fires as the clip starts.
- **One clip fires through a cross-fade:** the one that weighs more, so a fade never
  doubles a footstep. The clip fading out fires until the new one passes half the fade.
- **Rust reads them in the same tick,** in the `AnimationEvents` resource:

  ```rust
  fn tick(&mut self, world: &mut World, _dt: f32) {
      let Some(events) = world.resource::<AnimationEvents>() else { return };
      for e in events.fired() {
          if &*e.name == "step" {
              // e.entity stepped; e.data says which foot.
          }
      }
  }
  ```

  The list holds the last tick's events only. Characters come in the order of their
  entities, each one's events in the order played.
- **Scripts on the character hear them in the next tick,** as
  `on_animation_event(self, name, data)` (`data` is `nil` when empty):

  ```lua
  function Steps.on_animation_event(self, name, data)
      if name == "step" then
          self.steps += 1
      end
  end
  ```

- **An event's sound plays where the character stands,** as an `AudioSource3D` that
  despawns at its end. Not while the network replays ticks.
- **What it costs:** about 17 ns a character a tick on ref-1 (1,000 walkers: 17 µs);
  nothing for characters whose clips have no events. No allocation while events fire;
  a sound played allocates its source, as any sound spawned does.
- A pose the editor holds fires nothing, and nothing fires while the world stands still.

### Clips made in the editor: `.anim` files

A clip doesn't need a skeleton. A door opening, a lamp flickering, a platform going up and
down is a clip of keyed fields, an `.anim` file (ADR 0124's E12-4a). Any entity's
`Animator` plays one by name from its `clips`:

```rust
use engine_app::prelude::*;

let door = world.spawn((
    Transform::IDENTITY,
    Animator::new("open")
        .with_clip("open", open_file)
        .with_clip("shut", shut_file),
));
```

```lua
-- The door's trigger touched: it opens, or shuts again.
function Door.on_touch(self, other)
    self.Animator:play(if self.opened then "shut" else "open", 0.2)
    self.opened = not self.opened
end
```

The file is text in the scenes' grammar, one track per field it moves:

```text
[anim version=1 id=6f1c…]

length = 1
once = true
tracks["Lamp:PointLight.range"].keys.0.time = 0
tracks["Lamp:PointLight.range"].keys.0.value = [2]
tracks["Lamp:PointLight.range"].keys.0.curve = Step
tracks["Lamp:PointLight.range"].keys.1.time = 0.5
tracks["Lamp:PointLight.range"].keys.1.value = [8]
tracks["Transform.rotation"].keys.0.time = 0
tracks["Transform.rotation"].keys.0.value = [0, 0, 0]
tracks["Transform.rotation"].keys.1.time = 1
tracks["Transform.rotation"].keys.1.value = [0, 90, 0]
events.0.time = 0.9
events.0.name = "thud"
```

- **What a track moves:** `Component.field`, as a `Tween`'s target (`Transform.translation`,
  `Transform.scale.y`, `PointLight.color`), a number or up to four. A field of an entity
  below the animated one comes after its names: `Lamp:` for a child named Lamp,
  `Arm/Lamp:` for a grandchild.
- **A key** holds the field's numbers. A rotation is keyed as the inspector shows it,
  three Euler angles in degrees: the door's quarter turn is `[0, 90, 0]`, and a fan can
  turn past 180 degrees between two keys.
- **Curves:** a key eases into the next (`Smooth`, the default), goes straight
  (`Linear`) or holds until it (`Step`). Smooth keys never overshoot: a curve stays
  between its keys and is flat at a peak.
- `once = true` holds the clip's end, as for an imported clip; without it, the clip
  loops. Events work as an imported clip's (`on_animation_event`, sounds).
- **When it runs:** each tick, after the animators move on, so gameplay and scripts read
  the tick's values, saves keep them and the network sends them. A moved transform is drawn
  smoothly between ticks.
- **Through a fade** (`play(clip, fade)`), a field both clips key blends; one only the
  clip fading out keys stays where it was left.
- **A character with a model** plays its model's clips by their names; a name in `clips`
  plays the file instead. Such a clip can key the character's joints (below); the joints
  it does not key rest. Combining an own clip with a model's at once comes with graphs.
- **Joints** are keyed by tracks named `@` and the joint's name, then its part:
  `@mixamorig:Hips.rotation`, `@Skeleton_arm_joint_R.translation`, `@Hand.L.scale` (the
  part follows the last dot).
  - The keys are in the joint's parent's space, as the model's clips have them. A rotation
    is three Euler angles in degrees, like a field's (four numbers: a quaternion), and a
    scale is one number or three.
  - The pose is the model's rest pose with the keyed joints set over it. A joint the model
    lacks is left out, so a clip made for one model plays on another with the same names.
  - `play(clip, fade)` blends the poses across both kinds of clips: from a model's walk
    into a wave keyed in the editor and back. An own clip moves nothing by root motion,
    so the walk's motion fades out with it.
- **What it costs:** about 70 ns a door a tick with two tracks (1,000 doors: 70 µs on
  ref-1), no allocation, also when an animator switches between clips it played before.
- **While the editor edits** the world stands still and nothing is written: the door
  shows the values the scene holds. The Animation tab previews a clip on it (its
  playhead held or ▶), setting its fields for drawing only; the scene, its saves and undo
  keep their own values (the [editor guide](editor.md) has the keys). New Clip makes a
  clip; K, ◆ and an edit of a field the clip moves key it at the playhead (E12-4b-2). On
  a character the preview poses the joints the clip keys, and in Tab's joint mode the
  picked joint's rings turn it and key it (E12-4e-2).

### Animation graphs: `.agraph` files

A graph says which clips a character plays as its parameters change: idle until it moves, a
walk blended into a run by its speed, a jump when a script fires one (ADR 0124's E12-5). The
file comes first (E12-5a); the player plays graphs from E12-5c and the editor draws them
from E12-5d.

```text
[agraph version=1 id=…]

params["speed"].kind = Float
params["jump"].kind = Trigger
layers["base"].name = "Base"
layers["base"].root = "m0"
nodes["m0"].kind = Machine
nodes["m0"].entry = "idle"
nodes["c1"].clip = "Survey"
nodes["b2"].kind = Blend1d
nodes["b2"].param = "speed"
nodes["b2"].points.0.clip = "Walk"
nodes["b2"].points.1.at = [3.5, 0]
nodes["b2"].points.1.clip = "Run"
states["idle"].machine = "m0"
states["idle"].name = "Idle"
states["idle"].pose = "c1"
states["move"].machine = "m0"
states["move"].name = "Move"
states["move"].pose = "b2"
transitions["t1"].from = "idle"
transitions["t1"].to = "move"
transitions["t1"].when = "speed > 0.1"
```

- **Parameters** have a kind (`Float`, `Bool`, `Int`, `Trigger`) and a starting value.
- **Layers** play a node each, the smallest `order` first, normal or additive, by a
  weight, on the joints their mask names.
- **Nodes:** a clip by name, a blend of clips along one parameter or two, or a state
  machine. States show a node's pose; transitions go between a machine's states.
- **A transition** goes when its condition holds (and, with an `exit` time, once the state
  it leaves has played that much), blending over `blend` seconds (0.25), by a cross-fade or
  by inertialization. Without `from` it leaves any state of its machine.
- **Conditions** are expressions over the parameters:
  - `speed > 0.1 and grounded`, `not crouch or speed < 2`, `jump`, `(a + b) * 2 >= 3`;
  - comparisons `< <= > >= ==` and `~=` (or `!=`); `and`, `or`, `not` (or `&&`, `||`, `!`);
    arithmetic `+ - * /`; `true`, `false`;
  - `not` binds looser than a comparison, so `not speed > 1` means `not (speed > 1)`;
  - a trigger is used up by the transition that takes it;
  - a misspelt parameter, a missing value or parenthesis, or `0 < speed < 2` (compare once:
    write `0 < speed and speed < 2`) is reported with where it is.
- Ids are short random strings and names are labels, so renaming a state touches nothing
  else; lines at their defaults are left out and everything is sorted, so merges stay small.
- **From Rust** (the player does this for you from E12-5c): describe the graph
  (`engine_anim::GraphDesc`), cook it for a rig, give each character a `GraphState`:

```rust
use engine_anim::{AnimGraph, ClipInfo, GraphState, GraphWork};

let (mut graph, problems) = AnimGraph::cook(&desc, |name| clip_info(name), &skeleton);
graph.prepare(|k| clips.get(usize::from(k)), skeleton.rest()); // additive layers' references
let speed = graph.param("speed").unwrap();
let mut state = GraphState::new(&graph);
let mut work = GraphWork::new(skeleton.joint_count());
// Each tick: parameters set, transitions taken, phases moved on.
state.set(speed, 3.0);
graph.tick(&mut state, 1.0 / 64.0);
// Each frame: the pose between the last two ticks.
graph.pose(&state, alpha, |k| clips.get(usize::from(k)), skeleton.rest(), &mut work, &mut pose);
```

- **What it costs** (ref-1, a 52-joint humanoid, idle and a nine-clip 2D blend, an
  upper-body layer and an additive lean): about 0.1 µs a character a tick and 4.5 µs a frame
  on one thread; 10,000 characters' frames in 5.7 ms on 16 workers. Nothing is allocated
  after `GraphState::new` and `GraphWork::new`.

### Sockets: things that follow a joint

A `JointAttachment` on an entity right under a character makes it a socket on one of the
character's joints (ADR 0124's E12-1c, as Godot's `BoneAttachment3D`). What hangs under the
socket follows that joint:

```rust
use engine_app::prelude::*;

let hand = world.spawn((JointAttachment::new("Hand.R"), Transform::IDENTITY));
world.set_parent(hand, Some(character))?;
let sword = world.spawn((Model::new(sword_mesh, steel), Transform::from_xyz(0.0, 0.1, 0.0)));
world.set_parent(sword, Some(hand))?;
```

- Each tick, before transforms propagate, the player sets the socket's `Transform` to where
  its joint is in the character's pose then: the editor's held pose, else the animator's at
  its time, else the rest pose. Gameplay in the tick sees the sword where the hand is, and
  frames between ticks draw it smoothly, as every transform is drawn.
- The things under the socket keep their own transforms, in the joint's space: the sword
  a tenth of a metre along the hand.
- While the editor edits, sockets follow the pose it shows: P in place, the Clips'
  playhead, the animator's `time`.
- A joint name the skeleton lacks leaves the socket where it is.
- Sockets are saved like any entity; the joint is looked up by name once per rig.
- In the editor, a row dropped on a joint (Tab shows them) goes into that joint's
  socket, one made for it where the character has none; a dropped character file's meshes
  that hang under a joint come in sockets of their own.

A game that poses its characters itself still can: set the palettes in `Game::animate`,
as below. Cross-fades, layers, root motion and graphs come with the rest of E12-2 and
E12-5.

## 1. Importing and cooking

glTF skins and animations are imported with the scene (`import_gltf`): joints, inverse
bind matrices, up to 4 influences per vertex per `JOINTS_n` / `WEIGHTS_n` set (two sets:
8 influences), and translation, rotation and scale channels with step, linear and cubic
spline interpolation. Morph target weights are not imported yet.

```rust
use engine_assets::animation::{raw_clip, shell_distances, skin_skeleton};
use engine_anim::{CompressSettings, compress};

let scene = engine_assets::import_gltf(&bytes, Some(dir))?;
let skin = skin_skeleton(&scene, 0)?;               // skeleton + joint mapping + inverse binds
let walk = raw_clip(&scene, 1, &skin).unwrap();     // animation 1 on that skeleton
let (clip, stats) = compress(&walk, &skin.skeleton, &CompressSettings {
    error_threshold: 1e-4,                          // 0.1 mm in meters
    joint_shell_distances: shell_distances(&scene, 0, &skin),
    ..CompressSettings::default()
});
println!("{} -> {} bytes, error {:.1e}", stats.uniform_bytes, stats.compressed_bytes, stats.max_error);
```

`cook_scene` does this for you: every animation that moves a skin is compressed into the
scene's `CookedSkin::clips` and stored in the pack (`CookSettings::animation_error`,
default 0.1 mm). Packs with skins are version 2; the engine reads versions 1 and 2.

**The error bound.** The compressor measures the error the way you would see it: it puts
four points ("virtual vertices") around each joint, at the joint and at its *shell
distance* along its axes, moves them through the whole skeleton with the source animation
and with the compressed one, and keeps the largest distance under `error_threshold`, at
every sample and halfway between samples. The shell distance should be where the skin
is: `shell_distances` measures each joint's farthest skinned vertex. Choose the threshold
in your scene's units: 0.1 mm (`1e-4` in meters) is invisible on a character.

**How small?** About 5-15× smaller than the samples stored as floats (Fox's Survey:
83 kB → 5.5 kB). Clips whose keys do not lie on one grid (Fox's Run) are resampled finer
and compress less; key them on a fixed rate if you can.

## 2. A character's frame

```rust
use engine_anim::*;
use engine_core::math::Affine3A;

// Once per character: its poses, blender and outputs (sized to the skeleton).
let n = skeleton.joint_count();
let (mut walk_pose, mut run_pose, mut pose) = (Pose::identity(n), Pose::identity(n), Pose::identity(n));
let mut blender = Blender::new(n);
let mut model = vec![Affine3A::IDENTITY; n];
let mut palette = vec![[0.0f32; 12]; skin.skin_joints.len()];

// Every frame:
walk.sample(t, skeleton.rest(), &mut walk_pose);
run.sample(t * run.duration() / walk.duration(), skeleton.rest(), &mut run_pose);
blender.blend(skeleton.rest(), &[
    Layer { pose: &walk_pose, weight: 1.0 - speed, mask: None, mode: LayerMode::Normal },
    Layer { pose: &run_pose, weight: speed, mask: None, mode: LayerMode::Normal },
], &mut pose);
local_to_model(&skeleton, &pose, &Affine3A::IDENTITY, &mut model);
skinning_matrices(&model, &skin.skin_joints, &skin.inverse_bind, &mut palette);
```

- **Sampling** is stateless: any time, in any order, costs the same (about 0.3 µs for a
  25-joint pose on the reference CPU). Times past the end are clamped; loop with `%`.
- **Blending.** Normal layers are averaged by weight (rotations by normalized weighted
  sums). Below `Blender::threshold` of total weight the rest pose fills in, so a joint no
  layer drives stays at rest. A **mask** (`Some(&weights)`, one weight per joint)
  restricts a layer, for example to the upper body: a subtree of joint `j` is
  `j..skeleton.subtree_end(j)`.
- **Additive layers** add a pose's difference to a base on top: compress the clip with
  `additive: Some(AdditiveBase::FirstSample)` (or `RestPose`) and blend it with
  `LayerMode::Additive`; its weight scales it, a mask restricts it.
- **Root motion.** Walk cycles animated in place carry the character forward through
  their root joint. Take that motion out and move the character yourself:

  ```rust
  let motion = root_motion(&walk, &skeleton, root, previous_t, t, true, RootMotionMode::default());
  character.position += character.rotation * motion.translation;
  character.rotation *= motion.rotation;
  remove_root_motion(&mut pose, &walk, &skeleton, root, RootMotionMode::default());
  ```

  The default extracts horizontal translation and yaw; jumps stay in the pose. With
  `looping`, a `t` smaller than `previous_t` means the clip wrapped once.

## 3. Inverse kinematics

IK runs on the model-space pose and writes both the local rotations and the model
transforms, so solvers can be chained and the skinning matrices see the result. Targets
are in model space.

```rust
// A foot on uneven ground: hip, knee, ankle; the knee bends towards `pole`.
TwoBoneIk { start: hip, mid: knee, end: ankle, target: ground_point, pole: knee_front, weight: 1.0 }
    .solve(&skeleton, &mut pose, &mut model, &Affine3A::IDENTITY);

// A spine reaching (any chain length).
let mut fabrik = Fabrik::new(); // keep it with the character: no allocation after the first call
fabrik.solve(&skeleton, &mut pose, &mut model, &root, &spine_joints, hand_target, 1e-3, 16);

// The head looking at something, at most 60°.
LookAt { joint: head, forward: Vec3::Z, target: point_of_interest, weight: 0.8, max_angle: 1.05 }
    .solve(&skeleton, &mut pose, &mut model, &root);
```

- Two-bone IK is exact: a reachable target is reached, an unreachable one gets the limb
  stretched straight towards it (`IkOutcome::reached` says which).
- FABRIK iterates to a tolerance; out of reach, the chain points straight at the target.
- Bone lengths never change; parents with non-uniform scale are not supported.

## 4. Many characters

```rust
for_each_parallel(&jobs, &mut characters, 16, |c| c.update(&clips, dt));
```

Each character owns its poses and outputs; clips and skeletons are shared read-only.
1,000 Fox characters (two clips blended, local to model, matrices) take 0.13 ms on the
reference machine's 16 threads, 1.1 ms on one.

## 5. Skinned meshes on the GPU

```rust
use engine_render::skinning::{SkinnedMeshData, Skinning};

let mut skinning = Skinning::new(&device, &mut allocator, &bindless, DEPTH_FORMAT, 1 << 16, IN_FLIGHT)?;
// Once per mesh: its bind pose and influences (from a cooked mesh's `joints` / `weights`).
let skin_id = skinning.add_skin(&SkinnedMeshData {
    mesh: MeshData { positions: &m.positions, normals: &m.normals, tangents: &m.tangents, uvs: &m.uvs, indices: &m.indices },
    joints: &m.joints,
    weights: &m.weights,
    joint_count: skin.skin_joints.len() as u16,
})?;
// Once per character: a mesh of its own, drawn like any mesh.
let (id, mesh) = skinning.add_instance(skin_id, &mut geometry)?;
let scene_id = scene.insert(SceneInstance { mesh, material, world_from_object });
skinning.set_scene_instance(id, Some(scene_id)); // with a GpuScene (ADR 0064)

// Every frame:
skinning.set_pose(id, &palette, &world_from_object);   // the same transform the scene/draw uses
skinning.update_bounds(&mut geometry, Some(&mut scene));
let geo = geometry.begin_frame(&mut fg, &device, &mut allocator, frame)?;
let skins = skinning.begin_frame(&mut fg, &mut ring, &device, &mut allocator, &bindless, &geo, frame)?;
// ... the views' forward passes; after each one with motion vectors:
skins.add_motion_pass(&mut fg, &mut ring, &bindless, &geo, depth, &passes.motion.unwrap())?;
```

- **Pre-skinning:** one compute dispatch per skin skins all its posed instances (linear
  blend skinning, 4 or 8 influences) into their meshes in the pool, positions, normals
  and tangents. About 13 µs per 100k vertices on the RTX 4060 Ti, 0.2 ms on the Radeon
  iGPU. Nothing else in the renderer knows about skinning.
- **Motion vectors:** the forward pass only knows each instance's rigid motion; the
  motion pass redraws the skinned instances with depth test *equal* and writes each
  vertex's own motion from last frame's skinned position. It costs a vertex pass over the
  skinned meshes in views with motion vectors (TAA, upscalers) and nothing elsewhere.
- **Teleports:** call `teleport(id)` (and `GpuScene::teleport`) so the new pose does not
  smear from the old one.
- **Bounds:** each frame's box comes from the joints' matrices and their bind-pose boxes,
  so culling and shadows follow the animation.
- An instance not posed in a frame keeps its last pose (with no motion).

## 6. Measuring

- `cargo run -p engine_assets --release --example anim_bench`: compression of the sample
  models and the per-character CPU cost.
- `cargo run -p engine_render --release --example skin_bench`: skinning, motion pass,
  shadows and forward at 100k and 1M skinned vertices (`ENGINE_ADAPTER=radeon` for the
  iGPU).
- `bench/acl-compare`: our clips against ACL's on the same data (needs ACL and RTM
  clones, see its README).
- The test models (Fox, CesiumMan, RiggedFigure, RiggedSimple, SimpleSkin) come from
  `tools/fetch-test-assets.ps1`.
