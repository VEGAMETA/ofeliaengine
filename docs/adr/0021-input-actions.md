# 0021. Input actions: named actions, contexts, tick-accurate state

- Status: Accepted
- Date: 2026-09-26
- Research: [docs/research/input.md](../research/input.md)
- Related: [ADR 0005](0005-window-and-input-threads.md) (raw input thread),
  [ADR 0019](0019-networking-model.md) (user commands)

## Context

The team wants Godot's ease of use for input: game code asks about actions ("jump"), not
keys. Wide controller support comes later.

The engine also needs:
- **Input sampled per fixed simulation tick without losing short taps** (§7, late input
  sampling).
- **A compact, deterministic per-tick user command** for networking with prediction
  (ADR 0019).
- **Sub-tick timestamps** for shots and latency measurement.
- **Zero allocations per frame** (§5).

## Options

1. **Godot's model as is.**
   - String actions with bindings and a deadzone, global state.
   - Simple, but it lacks contexts and chords.
   - Taps shorter than a tick depend on timing.
2. **Unity- or Unreal-style assets:** action maps, contexts, modifiers, triggers.
   Complete, but many concepts and files for simple games.
3. **Godot's simplicity with the parts that remove boilerplate** (chosen):
   - typed and named actions;
   - contexts with priorities (Unreal);
   - chords with disambiguation (leafwing);
   - per-player device filters (Unity).
   - Nothing else yet: hold and tap interactions are left to game code or a later ADR.

## Decision

**`engine_input`:**
- **`InputMap`:**
  - actions by name, of kind `Button`, `Axis1d` or `Axis2d`;
  - a deadzone per action (Godot's 0.2 by default);
  - contexts with priorities; the default context is enabled.
- **Bindings:**
  - keys by physical position (layout-independent, from scan codes);
  - mouse buttons, wheel and motion;
  - gamepad buttons, triggers as buttons with strength, sticks with Godot's circular
    deadzone;
  - four-button and two-button composites (normalized diagonals);
  - chords of up to three modifier keys.

  Conflicts (the same input with the same modifiers driving two actions in one context)
  are rejected, also when rebinding.
- **Evaluation:**
  - enabled contexts in priority order;
  - an input bound in a higher context is hidden from lower ones while that context is
    enabled;
  - a chord that fires consumes its key for plain bindings.
- **Two views:**
  - `sample_tick` for the simulation;
  - `sample_frame` for rendering and UI.

  Each view latches presses since its previous sample. A tap shorter than a tick is
  pressed for exactly one tick, then released in the next. A key held across ticks
  releases in the tick it is let go.
- **Values** are quantized to 16.16 fixed point when sampled. Every press carries its
  position within the sampled interval (the sub-tick time).
- **`ActionState::encode` / `decode`** is the network user command:
  - a bitset of pressed actions, then zigzag varints of the pressed actions' values and
    press fractions;
  - nothing pressed costs one byte per eight actions;
  - the server decodes exactly the values the client simulated with.
- **Names and ids.** Game code uses `ActionId`, or Godot-style names
  (`input.tick().pressed("jump")`), which resolve through the map.
- **Local multiplayer:** a `DeviceFilter` per player's `Input`.
- **Windows devices:**
  - `platform::from_raw` converts `engine_platform::raw_input` events (Pause and Num
    Lock by virtual key);
  - `platform::XInput` polls up to four Xbox-compatible pads. Empty slots are checked
    one at a time every 500 ms, because `XInputGetState` on an empty slot is slow.

## Measurements

- **Unit tests:**
  - scan-code table;
  - short taps and sub-tick fractions, holds and releases, frame and tick independence;
  - composites and stick deadzones, mouse motion per view;
  - chords, context priority, trigger strength, name lookup;
  - conflicts and rebinding;
  - encode/decode equality over a sequence of ticks;
  - per-pad players.
- **Zero allocations** over 450 steady-state frames, while handling events, toggling a
  context, sampling both views, encoding and decoding.
- **Timing:** sampling cost is proportional to the number of bindings (tens), well below
  a microsecond in practice. A benchmark follows with the editor's input panel.

## Consequences

- **Follow-ups:**
  - text format for input maps (with the serialization ADR, §13);
  - the editor's Input Map panel with interactive rebinding;
  - `.d.luau` action names for scripts (§10);
  - GameInput and HID gamepads with an SDL-format mapping database (wide controller
    support, as the team asked);
  - hold and tap interactions;
  - rumble.
- **The simulation reads input only through `ActionState`,** which is what the server
  sees. Prediction and replays reproduce the same inputs.
