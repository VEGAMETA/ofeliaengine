# Scripting in Luau

Scripts give objects behaviour without Rust (ADR 0114). A script is a `.luau` file in your
project's `scripts/` folder. It declares components with hooks the engine calls, and you
give those components to entities, as you give them a `Model` or a `Transform`.

This page grows with the scripting steps. What works today is below. What comes next is at
the end.

## A first script

`scripts/door.luau`:

```luau
--!strict
local Door = component("Door", {
    lift = 2.5,                    -- metres
    speed = prop.number(2, { min = 0.1, tooltip = "Metres a second" }),
    closed = vector.zero,          -- where it rests, taken at start
})

function Door.on_start(self)
    self.closed = self.Transform.translation
end

function Door.on_tick(self, dt: number)
    local player = world.find("Player")
    local near = player ~= nil
        and vector.magnitude(player.Transform.translation - self.closed) < 3
    local goal = if near then self.closed + vector.create(0, self.lift, 0) else self.closed
    local t = self.Transform
    local to_goal = goal - t.translation
    local step = math.min(vector.magnitude(to_goal), self.speed * dt)
    if step > 0 then
        t.translation += vector.normalize(to_goal) * step
    end
end

return Door
```

Give it to an entity in a scene file, as any component's fields are written:

```text
[entity 6a0e…]
Name = "Front door"
Transform.translation = [4, 0, 0]
Door.lift = 3
```

A prop the file leaves out takes its default. Scripts load before the scene, so the scene
finds its components.

In the editor, select the entity, then:
- **Attach Script** (the script button above the scene tree) or **Add Component**
  (Ctrl+Shift+A): pick `Door`;
- or **New Script…**, which makes `scripts/<name>.luau` from a template, named after the
  entity, attaches it at once and opens it in `on_tick`.

The inspector shows the component's props and its file. A click on the file, or on the
script's mark at the end of the entity's row, opens the script.

From Rust, `player.add_script(door, "Door")` gives one with its defaults.

## Components and props

- **`component(name, props)`** declares a component. Its name starts with a capital, as
  components' names do. A file may declare several; what it returns is its module.
- **Props** are the component's data. Each instance has its own, and they live in the
  world, as a Rust component's fields do:
  - a number (`lift = 2.5`), a boolean (`open = false`) or a vector (`closed =
    vector.zero`): their default and type;
  - `prop.number(default, { min, max, step, tooltip, hidden })`, `prop.integer(…)`,
    `prop.boolean(…)`, `prop.vector(…)`: the same, with limits and help. A write beyond
    `min` or `max` is clamped, as the inspector does;
  - `prop.entity()`: another entity, `nil` at first.

  Strings come in a later step.
- **Props are laid out by name** (a Luau table's keys have no order), and the inspector
  shows them that way. A prop away from its default is marked, and its reset puts the
  default back.

## Hooks

| Hook | When |
|---|---|
| `on_start(self)` | once, before the instance's first tick |
| `on_tick(self, dt)` | every fixed tick (64 a second by default), before your game's `tick` |
| `on_frame(self, dt, alpha)` | every rendered frame, before your game's `present`; `alpha` is how far the frame lies between the last two ticks |
| `on_stop(self)` | once the component went from a living entity |

- **Hooks run only while the world simulates:** in a game, and in the editor's Play.
- **They run in the entities' order** (the order they were made in), the same on every
  machine.
- **Define only the hooks you need:** each one costs a call per instance.

## `self`

- **`self.speed`:** a prop, read and written in the world.
- **`self.Transform`, `self.Model`:** the entity's components by their names, live. A
  field written there (`self.Transform.translation = p`) is in the world at once. Vectors
  are Luau `vector`s, written whole. Rotations are `quat`s.
- **`self.entity`:** the entity. `self.entity.name` is its `Name`, and
  `self.entity.alive` tells whether it still exists.
- **`self.locals`:** a table of your own, kept between hooks and never saved: caches and
  counters, not state the game needs back after a load.
- In `on_stop` the component is gone, so its props are too. `self.entity` and
  `self.locals` are still there.

## The libraries

| Library | What |
|---|---|
| `world.find(name)` | the entity with that `Name` (the first made, of several), or `nil` |
| `time.now()`, `time.tick()`, `time.dt()` | the game's time in seconds, the tick's number, its length |
| `quat.identity()`, `quat.axis_angle(axis, angle)`, `quat.euler(x, y, z)` | rotations, in radians; `q * v` rotates a vector, `a * b` composes |
| `print(...)`, `warn(...)` | the engine's log, with the script's file and line |

Luau's own libraries (`math`, `string`, `table`, `vector`, `buffer`) are there too, in
Luau's sandbox.

## Changing a script while it runs

Save a script (Ctrl+S in the editor, or in another editor) and it runs again at once:

- **Its instances go on** with their props' values and `self.locals`; `on_start` does not
  run again. The editor says how long it took (well under a millisecond for a thousand
  instances).
- **Props you add, remove or change** move by name: a prop that keeps its name and type keeps
  its value, a new one takes its default. Undo still works across the change.
- **A script with an error** (it does not compile, or fails at its top) leaves the old
  version running; the Output says where.
- **An instance an error stopped** starts again when its script is saved.

## When something goes wrong

- **A runtime error stops that instance:** its hooks are called no more, and the entity
  keeps everything else (it still draws and collides). The log says where:
  `scripts/door.luau:14: attempt to … (Door.on_tick)`. In the editor's Output, a click on
  it opens the script at that line.
- **A script that does not load** (a syntax error) is in the log; the other scripts load.
- **A hook that runs too long** (over 50 ms: a loop that never ends) is stopped the same
  way.

## What it costs

Measured on ref-1 (Ryzen 7 7700, release):
- an empty `on_tick` costs 0.17 µs per instance;
- reading and writing a prop or a component's field costs about 36 ns each (a number, a
  boolean, a vector);
- 1,000 doors like the one above take 0.65 ms a tick.

A field read is a call into the engine, though a short one. `self.Transform` is made once
and kept, as are `self.entity` and `self.locals`. An entity from `world.find` and a
component reached through it are made at each access: keep them in a local within a hook.
Logic over thousands of objects every tick belongs in Rust.

## Types: in the editor and in VS Code

The editor knows the engine's API and your components: it underlines a wrong field or a
value of the wrong type as you write, completes `self.Transform.` and shows types on
hover. In a hook, `self` is your component's props with the entity's components
(`self.lift`, `self.Transform.translation`), typed from `component`'s declaration: write
`function Door.on_tick(self, dt: number)` and nothing more.

The editor writes the same types to `types/ofelia.d.luau` in your game's folder whenever
they change (a script declared, a component added). A game made with `ofeliaengine new`
has `.vscode/settings.json` pointing [luau-lsp](https://github.com/JohnnyMorganz/luau-lsp)
at it, so VS Code checks and completes your scripts the same way. The file is in
`.gitignore`: open the editor once after you clone to have it.

- A field named by one of Luau's reserved words (`Timer.repeat`) has no type: read it as
  `timer["repeat"]`.
- Lists, maps and asset references are not readable from scripts yet, and have no types.

## Coming next

- script components in saves;
- systems over queries, `input`, `world.spawn`, `world.query`, entity methods (`has`, `add`,
  `remove`, `despawn`);
- `send`, `after` timers, `audio`, `random`, physics queries and triggers.
