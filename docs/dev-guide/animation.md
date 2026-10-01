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
