# Input: actions

Game code does not ask "is W pressed?". It asks "is the player moving forward?". You
name **actions** ("jump", "move", "fire") and **bind** keys, mouse and gamepad inputs to
them. Players can rebind them; your code does not change.

## 1. Define actions

```rust
use engine_input::{Binding, GamepadButton, InputMap, Key, MouseButton, Stick};

let mut map = InputMap::new();

let jump = map.add_button("jump")?;
map.bind(jump, Binding::button(Key::Space))?;
map.bind(jump, Binding::button(GamepadButton::South))?;   // A on Xbox, Cross on PlayStation

let walk = map.add_axis2d("move")?;
map.bind(walk, Binding::wasd())?;                          // diagonals are not faster
map.bind(walk, Binding::stick(Stick::Left))?;              // with a circular deadzone

let look = map.add_axis2d("look")?;
map.bind(look, Binding::mouse_motion(0.1))?;               // sensitivity
map.bind(look, Binding::Stick { stick: Stick::Right, scale: 4.0 })?;

let save = map.add_button("save")?;
map.bind(save, Binding::chord(&[Key::ControlLeft], Key::KeyS))?; // does not also press S
```

- **Keys are named by position** on a US keyboard (`KeyW`, `Space`). `KeyW` is the key
  above `KeyS` on every layout, so WASD works on AZERTY too.
- **Binding the same input to two actions** in one context is an error. The same key
  with a modifier (`Shift+Space`) is a different input.

## 2. Read actions

```rust
use engine_input::Input;

let mut input = Input::new(map);

// Every frame, feed the events (the engine does this for you).
input.handle(&event);

// Every simulation tick:
let tick = input.sample_tick(now);
if tick.just_pressed(jump) { /* start the jump */ }
let [x, y] = tick.vector(walk);             // −1..1, +y is forward/up
let [dx, dy] = tick.vector(look);           // mouse counts × sensitivity this tick

// Or by name, like Godot:
if input.tick().pressed("jump") { /* ... */ }
```

| Method | Meaning |
|---|---|
| `pressed` | held now (axes: moved past the deadzone) |
| `just_pressed` | pressed in this tick, not in the previous one |
| `just_released` | released in this tick |
| `strength` | 0..1 for buttons (analog triggers give partial values) |
| `axis` | −1..1 for one-dimensional axes |
| `vector` | `[x, y]` for two-dimensional axes |
| `press_fraction` | when in the tick the press happened (0..1) |

- **Ticks never lose a press.** A tap shorter than one tick is `pressed` and
  `just_pressed` for exactly one tick.
- **Rendering and UI:** use `input.sample_frame(now)` and `input.frame()`. They see
  presses as soon as a frame is drawn, independent of the tick rate.

## 3. Contexts: menus, vehicles, modes

```rust
let driving = map.add_context("driving", 10)?;   // higher priority than the default (0)
let brake = map.add_button("brake")?;
map.bind(brake, Binding::button(Key::Space))?;   // bound in "driving"
// ...
input.set_context_enabled(driving, true);        // Space now brakes instead of jumping
```

- While a context is enabled, the inputs it binds are hidden from lower-priority
  contexts.
- Inputs it does not bind keep working: the mouse still aims.
- Enabling and disabling costs nothing measurable. Do it whenever the mode changes.

## 4. Rebinding

```rust
input.edit_map(|map| map.rebind(jump, 0, Binding::button(Key::KeyF)))?;
```

`rebind` refuses a conflict and tells you which action already uses the input, so your
settings menu can ask the player what to do.

## 5. Local multiplayer

Give each player their own `Input` and the devices they use:

```rust
use engine_input::DeviceFilter;

player_one.set_devices(DeviceFilter { keyboard_mouse: true, gamepads: 0b01 });
player_two.set_devices(DeviceFilter::gamepad(1));
```

## 6. Networking

A tick's state is the command the client sends to the server (see the networking model,
ADR 0019):

```rust
let mut bytes = Vec::new();
tick.encode(&map, &mut bytes);      // a few bytes; one bit per released action
server_state.decode(&map, &bytes)?; // exactly the values the client used
```

Values are quantized when sampled, so a predicting client and the server compute with
identical numbers.

## 7. Gamepads

Xbox-compatible controllers work through XInput, up to four at a time. Wider controller
support (PlayStation, Switch, generic HID pads with automatic button mapping) is planned.
