# ADR 0127: Project settings (input map, navigation, physics, lighting)

Status: **Accepted** (team decision, 2026-11-01: 1а, 2а, 3а, 4а, 5а, every recommendation).

## Context

E14's ship-first items after the one-button build (ADR 0126): the input map and the
navigation, physics and lighting settings. A game sets these in Rust today
(`PlayerSettings::input`, the physics world's defaults), a scene file's game has no
navigation mesh (only a cooked scene pack has one, ADR 0103), and a player cannot rebind keys
in a way the game keeps. Research: `docs/research/ux/project-settings.md` (Godot, Unity,
Unreal).

## Options and the recommendation

1. **Files:** (а) one file per area in `settings/` (`input.settings`, `physics.settings`,
   `navigation.settings`), the scene grammar (kind, version, lines), edited together in one
   **Project Settings** tab; (б) one file for everything at the root (Godot's
   `project.godot`). **Recommended: а** — a team's input and physics changes never touch the
   same file; the tab still shows them in one place.
2. **Player's rebinding:** (а) the authored map stays as written, the player's changes are
   overrides saved in the save folder (`input.user`), loaded over it at start, with an API
   (`input:rebind(action, binding)`, `input:reset()`) and the listen-for-input capture; a
   ready-made rebinding screen later (with the in-game UI's widgets); (б) the game writes its
   own. **Recommended: а.**
3. **Navigation mesh:** (а) the editor bakes each scene's mesh in the background as it is
   edited (it already does, to draw it) and keeps it beside the scene (`<scene>.navmesh`,
   committed); Play and builds read it; scripts get `nav.path(from, to)` and
   `nav.closest(point)`; agents' settings in `navigation.settings`; (б) baked at build time
   only, Play baking at load. **Recommended: а** — what Play walks is what ships, no wait at
   load.
4. **Physics settings:** gravity, the fixed tick rate, the solver's steps, and **named
   collision layers with a matrix** of which collide (up to 32); bodies name their layer.
   (а) all of these now; (б) gravity and tick rate now, layers later. **Recommended: а** —
   layers exist in the physics API already (queue item 3); naming them is what the editor
   lacks.
5. **Lighting:** (а) per scene (the sun, sky, fog, exposure, GI as the scene's components, as
   now), the project holding the defaults a new scene starts from (`lighting.settings`);
   (б) project-wide lighting. **Recommended: а** (all three engines).

## Phases (with the recommendations)

| Step | What |
|---|---|
| **S1** | the input map: `settings/input.settings` (contexts, actions, kinds, deadzones, bindings), the player reading it into `engine_input::InputMap`, scripts' completion of action names from it |
| **S2** | the editor's Project Settings tab: the Input page (actions, contexts, bindings, listen to bind) |
| **S3** | the player's overrides (`input.user`), the rebinding API, listen-for-input in game |
| **S4** | physics settings (gravity, tick, steps, named layers, the matrix) in the file, the player, the tab |
| **S5** | navigation: agents' settings, `<scene>.navmesh` baked by the editor, read by Play and builds, the scripts' paths API (ADR 0126's G3c lands here) |
| **S6** | lighting defaults for new scenes |

## Consequences

- Rust games keep setting `PlayerSettings` by hand; a file overrides nothing a game set in
  code, and a game with neither has the engine's defaults.
- The build copies `settings/` as always-in (ADR 0126); `.navmesh` files are reached from
  their scenes.

## Questions for the team

1. Files: one per area in `settings/` (а) or one file for everything (б)?
2. Player's rebinding: overrides saved apart + API (а) or the game's own (б)?
3. Navigation: baked by the editor into `<scene>.navmesh` (а) or at build time only (б)?
4. Physics: gravity, tick, steps and named layers with a matrix now (а), or layers later (б)?
5. Lighting: per scene with project defaults (а) or project-wide (б)?

## As built (S1, 2026-11-01): the input map in `settings/input.settings`

- **Bindings as text** (`engine_input::text`): `Binding::to_text` and `Binding::parse`,
  and `ButtonSource::to_text` / `parse`:
  - a key by its name (`KeyW`), with modifiers before it (`ControlLeft+KeyS`);
  - `Mouse.Left`, `Pad.South`, `Pad.LeftTrigger+` / `Pad.LeftX-`, `Wheel.Up`;
  - two buttons for a 1D axis (`KeyA / KeyD`), an analog axis (`Axis.LeftY*-1`);
  - four buttons for a 2D axis (`KeyW, KeyS, KeyA, KeyD`: up, down, left, right);
  - a stick (`Stick.Left*0.5`) and mouse motion (`MouseMotion*0.1`).

  Every kind round-trips, and bad text says what it could not read.
- **`engine_app::project_settings::InputSettings`** (`settings/input.settings`, kind
  `input`, version 1) is one value in the scenes' grammar:
  - `contexts` by name (priority, enabled, blocks_lower), besides the implicit `default`;
  - `actions` by name (kind `Button` / `Axis1d` / `Axis2d`, deadzone, and `bindings` by
    context as lists of binding texts).

  Lines at their defaults are left out, except an action's kind.
- **`InputSettings::to_map`** builds an `engine_input::InputMap`: contexts by priority,
  actions in name order, each binding bound in its context. A binding that names no input,
  does not fit its action or conflicts, and a context that does not exist, are left out
  and returned as problems (the player logs them).
- **The player:** with no `PlayerSettings::input` set in code and a project folder, it
  reads the project's `settings/input.settings` at start. A game's own map wins.
- **Tests:**
  - every binding kind round-trips, and bad text is an error;
  - the file round-trips without its defaults;
  - the map it makes (kinds, bindings per context, a bad binding left out and said);
  - an offscreen player with only the file sees "jump" on Space.
- **Left:**
  - Luau completion of action names from the file;
  - the Project Settings tab (S2);
  - the player's overrides and rebinding API (S3);
  - physics, navigation, lighting (S4–S6).
