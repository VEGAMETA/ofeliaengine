# The editor

The editor runs inside your game's own build (ADR 0107): the player runs the game's world,
draws the scene into the viewport panel, and the editor's shell (the team's Godot layout)
is drawn over the frame. Your components and systems are native in it.

```text
cargo run -p engine_editor --release --bin editor -- scenes/main.scene --project C:/games/mygame
```

- **`scene`**: a scene file (ADR 0108), relative to `--project` (the working folder by
  default). Without one, an empty world.
- **`--frames N`** renders N frames and exits (smoke tests), printing the time with the
  start.

From a game's own build:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The game's settings; their `scene` is what the editor opens.
    engine_editor::run(settings()?, start)
}
```

`start` makes the game from the player when Play starts (Play comes with phase E3).

## What works now

- **The viewport** shows the scene through the game's renderer (every feature the player
  has), from the editor's own camera, in the panel's rectangle.
- **The scene tree** lists the scene's entities (those with a scene id) in their hierarchy:
  - each row shows its name (`Name`) and its kind (camera, sun, light, model);
  - a click selects a row; a click on the selected row opens or closes it.
- **The inspector** shows the selected entity's components as the scene file writes them,
  assets by path (`builtin#cube`, `materials/red.material`). Editing comes next (phase
  E1-4).
- **Edit mode:** the game is not made and nothing ticks.

## Viewport navigation

| Action | Input |
|---|---|
| Orbit around the pivot | middle drag |
| Pan | Shift + middle drag |
| Zoom toward the pivot | mouse wheel |
| Look around (the pivot follows) | right drag |

The bindings follow Godot (`docs/research/ux/viewport-navigation.md`). Flying with
right button + WASD and focusing the selection come later.

## Coming next

| Phase | What |
|---|---|
| E1-4 | the tree and the inspector edit the world (commands, undo, multi-editing), saving the scene |
| E1-5 | sessions, autosave, rebuild and restart after a code change |
| E2 | selection in the viewport, gizmos, snapping, placement by drag and drop |
| E3 | Play on a copy of the world |

The whole plan is ADR 0107 §7.

## HDR

The editor presents in SDR for now: it draws through the player, which presents in SDR.
HDR output comes back with the player's own HDR output, which games get too.

## What it costs (ref-1, release, 1600×900)

| | |
|---|---|
| Start and 60 frames | 0.51–0.69 s (budget for the start alone: 2 s) |
