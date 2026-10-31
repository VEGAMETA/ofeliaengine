# Project settings: input map, navigation, physics, lighting (E14)

What a game developer sets once for the whole game, and the player sometimes changes: which
inputs drive which actions, how agents walk, how bodies fall and collide, how scenes are lit.
Today our games set all of it in Rust (`PlayerSettings::input`, the physics world's own
defaults, a navmesh only from a cooked scene pack). E14 brings it into files the editor edits.

Checked 2026-11-01 against the products as they are (Godot 4.4, Unity 6, Unreal 5.5), from
their documentation and our use of them; no code read.

## Godot

- **One file, `project.godot`**, an INI-like text file at the root, every setting in its
  section (`[input]`, `[physics]`, `[navigation]`, `[rendering]`). The Project Settings window
  edits it; the Input Map tab lists actions, each with its events (key, mouse button, joypad
  button/axis) and a deadzone; "Listen for input" captures a key.
- **Rebinding in game:** `InputMap.action_erase_events` / `action_add_event` at run time; saving
  the player's choice is the game's own job (usually a `ConfigFile` in `user://`).
- **Navigation:** a `NavigationRegion3D` node holds a `NavigationMesh` resource with the agent's
  radius, height, max climb and slope; baked in the editor (a button) into the resource, saved
  with the scene or as a `.res`. Project settings hold only defaults (cell size, edge
  connection margin).
- **Physics:** gravity, the physics tick rate, layer names (32 per kind) in project settings;
  which layers a body is in and collides with is per body (two bit masks).
- **Lighting:** per scene, a `WorldEnvironment` node (sky, ambient, fog, tone mapping, glow);
  the project holds a default environment.
- **What annoys:** one file for everything conflicts often in teams (every setting change
  touches it); layer collisions as per-body masks are easy to get wrong (no matrix view).

## Unity

- **Input System:** an `.inputactions` asset (JSON) per set of actions: action maps (our
  contexts), actions, bindings with composites (WASD) and processors; an editor window with
  three columns (maps, actions, binding properties) and "Listen" to capture; a C# class can be
  generated from it.
- **Rebinding:** `PerformInteractiveRebinding()` and `SaveBindingOverridesAsJson()`: the asset
  stays as authored, the player's changes are overrides saved apart, loaded at start.
- **Navigation:** the AI Navigation package: a `NavMeshSurface` component per area, agent types
  (radius, height, step, slope) in the Navigation window, baked in the editor into a NavMesh
  data asset beside the scene.
- **Physics:** Project Settings > Physics: gravity, the layer collision matrix (a triangle of
  checkboxes), solver iterations; the fixed timestep in Time.
- **Lighting:** per scene, a Lighting Settings asset (environment, baking) and the Lighting
  window; skybox and ambient per scene.
- **What annoys:** many separate project-settings assets (ProjectSettings/*.asset) nobody
  opens but through the window; the matrix is clear.

## Unreal

- **Enhanced Input:** Input Action assets (value type, triggers, modifiers) and Input Mapping
  Context assets (keys to actions, with priorities), added to the player at run time; a
  per-user `EnhancedInputUserSettings` saves remaps by key profile.
- **Navigation:** a Nav Mesh Bounds Volume in the level and the RecastNavMesh actor's
  settings; agents in Project Settings > Navigation System; built in the editor (or at run time
  when dynamic), stored in the level.
- **Physics:** Project Settings > Collision: object channels, trace channels and presets
  (named rules of block/overlap/ignore per channel); gravity in World Settings or project.
- **Lighting:** per level (Sky Atmosphere, Directional Light, Post Process Volume).
- **What annoys:** settings spread over INI files and assets; presets are powerful but deep.

## What we take

- **Separate files per area, one Project Settings place in the editor** (Unity's split, so a
  team's changes to input and to physics don't conflict; Godot's one window to find them).
  In our scene grammar, so they diff and merge line by line, each with its version.
- **Input as Unity and Unreal keep it:** contexts with priorities (our `InputMap` has them
  already), actions with kinds, bindings with composites and chords; **the authored map
  stays, the player's changes are overrides saved apart** (in the save folder), loaded over
  it at start.
- **A listen-for-input field** to bind (all three have one): press the key, it is bound.
- **Physics:** gravity, the tick rate and **named collision layers with a matrix** (Unity's
  view of Godot's layers; our layers exist in the physics API, ADR 0066 / queue item 3).
- **Navigation:** agent settings in the project, the mesh baked by the editor into a file
  beside the scene (Unity's and Godot's), read by Play and by builds; scripts get paths.
- **Lighting stays in the scene** (all three): the project holds the defaults a new scene
  starts from.

## What we don't

- One file for everything (merge conflicts), generated code from the input asset (our
  scripts name actions by string with completion from the file), Unreal's collision presets
  for now (the matrix first).
