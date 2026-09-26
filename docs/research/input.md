# Input: actions, bindings, contexts

Checked on 2026-09-26.

The team asked for Godot's simplicity for game developers ("jump" is an action; code asks
`is_action_pressed("jump")`). Wide controller support comes later. The input system also
feeds networking (ADR 0019): one tick's actions are the user command sent to the server,
with sub-tick timestamps.

No code was copied. Godot and leafwing are MIT/Apache and were read for design; Unity
and Unreal are studied as products.

## Sources

| Source | Version / date | License | How it was studied |
|---|---|---|---|
| Godot `core/input/input_map.{h,cpp}`, `input.{h,cpp}`, `input_event.h` | master `4617300` (2026-09-25) | MIT | read |
| leafwing-input-manager (Bevy) | main, README | MIT / Apache-2.0 | read |
| Unity Input System (actions, bindings, composites, processors, interactions, control schemes) | 1.x | proprietary | known product behavior, docs host unreachable |
| Unreal Enhanced Input (Input Actions, Mapping Contexts with priorities, Modifiers, Triggers) | UE 5.x | proprietary (source-available) | known product behavior, docs host unreachable |
| SDL GameControllerDB mapping format | SDL 2/3 | zlib | known format; for the later controller work |
| `engine_platform::raw_input` | this repository | ours | read (timestamped raw keyboard and mouse on a time-critical thread) |

## How they work

### Godot

- **`InputMap`:** named actions (`StringName`), each with a deadzone (0.2 by default,
  0.5 for toggles) and a list of input events that trigger it:
  - a key by physical code, key code or label;
  - a mouse button;
  - a gamepad button;
  - a gamepad axis with a direction.

  Project Settings has an Input Map editor; `InputMap` can rebind at run time.
- **`Input` queries:**
  - `is_action_pressed`, `is_action_just_pressed`, `is_action_just_released`;
  - `get_action_strength` (0–1 after the deadzone) and the raw strength;
  - `get_axis(negative, positive)`;
  - `get_vector(-x, +x, -y, +y, deadzone)`: a circular deadzone remapped to 0–1, then
    clamped to length 1.
- **"Just pressed" is tracked per physics frame and per process frame,** so a physics
  tick and a render frame each see a press once.
- **Events also propagate through the scene tree** (`_input`, `_unhandled_input`), so UI
  consumes clicks before gameplay.
- **Strengths:**
  - it is simple and discoverable: actions are strings and the editor lists them;
  - two lines of code give movement with a proper deadzone.
- **Weaknesses:**
  - actions are untyped strings: a typo fails silently at run time;
  - no contexts: menus and vehicles enable and disable actions by hand;
  - no chords with disambiguation;
  - `just_pressed` of a tap shorter than a physics tick depends on timing;
  - one global state, so local multiplayer means per-device filtering by hand.

### Unity Input System

- **Actions** have a type (button, value, pass-through). **Bindings** have:
  - **composites:** 2D vector from WASD, 1D axis, button with modifiers;
  - **processors:** deadzone, invert, scale, normalize;
  - **interactions:** hold, tap, slow tap, multi-tap.
- **Action maps** group actions (gameplay, UI) and are enabled or disabled as a set.
  **Control schemes** (keyboard + mouse, gamepad) and **player input** assign devices to
  local players.
- **Rebinding** is interactive, with overrides saved as JSON.
- **Strengths:** composites and processors cover every common case declaratively;
  device-to-player assignment is built in.
- **Weaknesses:** several layers of assets and concepts. Callbacks, polling and events
  are three ways to read the same action, which confuses newcomers.

### Unreal Enhanced Input

- **Input Actions** have a value type (bool, 1D, 2D, 3D). **Mapping contexts** map
  keys to actions with **modifiers** (dead zone, negate, swizzle, scale) and
  **triggers** (pressed, released, hold, tap, chorded action).
- **Contexts are added and removed at run time with priorities.** A vehicle context
  above the on-foot context overrides the shared keys while you drive.
- **Strengths:** contexts with priorities solve modes cleanly; the value type is part of
  the action.
- **Weaknesses:** everything is an asset: many small files for simple games.

### leafwing-input-manager (Bevy)

- **Actions are a Rust enum** (typed; typos are compile errors).
- **An `InputMap` per entity** gives local multiplayer; an `ActionState` component
  holds the state.
- **Also provided:**
  - chords with a clash strategy (Ctrl+S does not also fire S);
  - virtual D-pads, axis processing;
  - UI buttons that press actions;
  - input mocking for tests;
  - `ActionDiff` for sending action changes over the network.

## What we take, what we do differently

1. **Actions are named in one place,** as in Godot.
   - Game code gets both forms:
     - the Godot-style string API: `input.pressed("jump")`, which resolves once and
       caches;
     - typed handles: `ActionId`, for hot code and generated constants.
   - Scripts get the names in the generated `.d.luau` file, so a typo is an editor
     error, not silence.
2. **Value types per action,** as in Unreal and Unity:
   - `Button` (pressed and strength);
   - `Axis1d` (from an analog axis or a negative/positive pair, Godot's `get_axis`);
   - `Axis2d` (from a stick, four buttons or the mouse, Godot's `get_vector`).

   Godot's circular deadzone and remapping is the default for sticks.
3. **Contexts with priorities,** as in Unreal:
   - gameplay, UI, vehicle, editor;
   - a higher context that binds a key consumes it, so lower contexts do not see it;
   - enabling and disabling a context is one call;
   - Godot's manual toggling of actions goes away.
4. **Chords with disambiguation,** as in leafwing: Ctrl+S consumes S, so "save" does not
   also fire "move back".
5. **Tick-accurate, never lost** (improving on Godot).
   - Presses and releases between two simulation ticks are latched: a tap shorter than a
     tick still shows up as `just_pressed` then `just_released` in the next tick.
   - Rendering and UI read a separate per-frame view.
   - Mouse deltas are summed per tick in device counts, not scaled by frame time.
6. **Sub-tick timestamps.**
   - Every press carries its raw-input time (the raw input thread already stamps
     events).
   - The tick state exposes the fraction within the tick. Networking uses it for shots
     and lag compensation (ADR 0019); the latency overlay uses it too.
7. **One tick of action state is the network user command.**
   - Buttons pack into a bitset, axes into quantized integers, deterministically.
   - Changes between ticks encode small (leafwing's `ActionDiff` idea).
8. **Local multiplayer by players, not by global state** (Unity, leafwing): each player
   has an action state and a set of devices.
9. **Rebinding at run time** with conflict detection, saved as user overrides over the
   project's defaults. The editor's Input Map panel uses the same API.
10. **Devices.**
    - Keyboard and mouse come from `engine_platform::raw_input` (layout-independent
      physical keys).
    - Gamepads come next: XInput first (in Windows), then GameInput and HID with an
      SDL-format mapping database for wide controller support (later, as the team
      asked).
11. **Performance.**
    - Zero allocations per frame.
    - No locks: the raw input thread already hands events over through a lock-free
      ring.
    - Binding lookup is a dense table from physical input to bindings.
