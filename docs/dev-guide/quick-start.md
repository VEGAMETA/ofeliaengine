# Quick start: a cube on screen in five minutes

From a fresh machine to a red cube turning on a floor, with a camera you fly with WASD and
the mouse. Then where to go from there.

## 1. Install (once)

- **Windows 11 x64** with a current GPU driver: Vulkan 1.3, which every NVIDIA, AMD and
  Intel driver of the last years has.
- **Visual Studio Build Tools** with the C++ workload (MSVC and the Windows SDK). The
  engine compiles some C and C++ libraries (Jolt physics, FidelityFX, Opus).
- **[rustup](https://rustup.rs)**. The engine's pinned Rust version installs itself on the
  first build.
- **[NASM](https://nasm.us)** 2.14 or later on `PATH`: it assembles the software AV1
  decoder.
- **Git**, and the engine:

  ```text
  git clone https://github.com/VEGAMETA/ofeliaengine
  ```

## 2. Start a game

```text
cd ofeliaengine
cargo run --release -p engine_cli -- new ../mygame
```

`ofeliaengine new` makes a folder with a whole game in it:

| File | What it is |
|---|---|
| `src/main.rs` | the game: a cube on a floor, the sun, a camera |
| `Cargo.toml` | the game's crate, depending on the engine's player (`engine_app`) |
| `.cargo/config.toml`, `rust-toolchain.toml` | the engine's linker, instruction set and Rust version |
| `sounds/` | your sound files ([audio.md](audio.md)) |

## 3. Run it

```text
cd ../mygame
cargo run --release
```

- **The first build compiles the engine:** 74 s on the reference machine (Ryzen 7 7700).
- **After an edit,** your game rebuilds in about 2.5 s.

![The quick start's game](quick-start.png)

A red cube turns on a floor. Fly around it: **WASD** moves, the **mouse** looks.

## 4. The code

`src/main.rs` is the whole game, in three parts.

**The settings:** the window and the controls. Actions have names; keys are bound to them
([input.md](input.md)).

```rust
let mut controls = InputMap::new();
let walk = controls.add_axis2d("move")?;
controls.bind(walk, Binding::wasd())?;
```

**The scene:** a mesh and two materials go to the GPU; entities place them in the world.

```rust
let mesh = setup.geometry.add_mesh(&cube().as_data())?;
let paint = setup.materials.add(MaterialDesc::solid([0.8, 0.15, 0.1], 0.0, 0.4));
let cube = setup.world.spawn((Transform::from_xyz(0.0, 0.0, 0.0), GlobalTransform::IDENTITY, MeshRenderer::new(mesh, paint)));
```

The sun is an entity with a `Sun` component; the camera, one with a `Camera`.

**The game:** two hooks the player calls ([player.md](player.md)).
- **`tick`** runs 64 times a second, the simulation; here the cube turns. Rendering
  interpolates between ticks, so motion is smooth at any frame rate.
- **`present`** runs every frame, after the ticks. What must follow the player's hand at
  once goes here; here the camera follows the mouse and the keys.

Everything comes from one line: `use engine_app::prelude::*;`.

## 5. Change it

- **Its colour:** the first three numbers of `MaterialDesc::solid` are red, green, blue.
- **Its speed:** `Quat::from_rotation_y(dt)` turns it a radian a second; double it.
- **More cubes:** spawn more entities with the same mesh. A thousand cost nothing worth
  measuring.
- **A lamp:** an entity with
  `PointLight::new(Vec3::new(1.0, 0.8, 0.6) * 10.0, 6.0)` and a `GlobalTransform` where it
  shines.

## 6. Your own scene

Model in Blender (or anything that writes glTF), then cook it with the engine's command
line (in the engine's folder, `cargo run --release -p engine_cli --` stands for
`ofeliaengine`):

```text
ofeliaengine cook level.gltf -o level.pack
```

and give the pack to the player:

```rust
PlayerSettings { pack: Some("level.pack".into()), ..settings }
```

- **Its objects become entities** you can move and despawn.
- **Its meshes and textures** stream within the GPU's memory.
- **Its colliders and navigation mesh** are cooked in ([assets.md](assets.md),
  [physics.md](physics.md), [navigation.md](navigation.md)).

## 7. Next

| To | Read |
|---|---|
| know what the player does for you | [player.md](player.md) |
| read the keyboard, mouse and gamepads | [input.md](input.md) |
| entities, components, queries | [ecs.md](ecs.md) |
| materials, lights, shadows, reflections | [rendering.md](rendering.md) |
| bodies that fall and collide | [physics.md](physics.md) |
| sound and music | [audio.md](audio.md) |
| saving and loading | [saves.md](saves.md) |
| playing together | [networking.md](networking.md) |
| text in many languages | [localization.md](localization.md) |
| where the frame's time goes | [profiling.md](profiling.md) |

All the guides: [README.md](README.md).
