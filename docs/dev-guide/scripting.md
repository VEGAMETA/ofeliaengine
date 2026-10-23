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
| `on_touch(self, other, point, normal)` | a body began touching the entity's body or its `Trigger`: `other` is its entity (`nil` for a body without one), the normal points toward it; at the start of the next tick |
| `on_untouch(self, other)` | it stopped touching |
| `on_stop(self)` | once the component went from a living entity |

- **Hooks run only while the world simulates:** in a game, and in the editor's Play.
- **They run in the entities' order** (the order they were made in), the same on every
  machine.
- **Define only the hooks you need:** each one costs a call per instance.
- **Contacts need a physics world** (the game's, or the player's own: see the libraries
  below). A `Trigger` component makes an entity a sensor: a box, a sphere or a capsule
  around it, that pushes nothing and hears what comes in.

```luau
-- A coin: on an entity with a `Trigger`, picked up by the player.
local Coin = component("Coin", { value = 1 })

function Coin.on_touch(self, other: Entity?, point: vector, normal: vector)
    if other and other.name == "Player" then
        print("picked up", self.value)
        self.Transform.translation = vector.create(0, -100, 0) -- out of the way
    end
end

return Coin
```

## `self`

- **`self.speed`:** a prop, read and written in the world.
- **`self.Transform`, `self.Model`:** the entity's components by their names, live. A
  field written there (`self.Transform.translation = p`) is in the world at once. Vectors
  are Luau `vector`s, written whole. Rotations are `quat`s.
- **`self.entity`:** the entity. `self.entity.name` is its `Name`, and
  `self.entity.alive` tells whether it still exists.
- **An entity is a value:** two that name the same entity are equal (`==`, even
  `rawequal`), and an entity is a table key like a number (`seen[other] = true`).
  `typeof(e)` is `"Entity"`. Making one costs nothing.
- **`self.locals`:** a table of your own, kept between hooks and never saved: caches and
  counters, not state the game needs back after a load.
- In `on_stop` the component is gone, so its props are too. `self.entity` and
  `self.locals` are still there.

## Entities and their components

An entity is a value: two that name the same entity are equal, and an entity is a table
key like a number. What it has:
- `e.name`: its `Name`, or `nil`;
- `e.alive`: whether it still exists;
- `e.parent`: its parent, or `nil`;
- `e.Transform`, `e.Wallet`: its components by name, live. Reading one it lacks is an
  error: check with `e:has(Transform)`.

Its methods (`self` has them too: `self:despawn()`):

| Method | What |
|---|---|
| `e:has(C)` | whether it has component `C` |
| `e:add(C, values?)` | gives it `C` (the default, then `values` such as `{ size = vector.one }`) unless it has one. At once: the next line reads it. A script component's hooks start at the next stage |
| `e:remove(C)` | takes `C` away, at once |
| `e:despawn()` | despawns it with its children at the end of the stage; their `on_stop` runs first |
| `for child in e:children() do` | its children, in order |
| `e:find_child(name)` | the nearest of its descendants named so (its children first), or `nil` |

`C` names a component:
- `Transform` and the game's other components are globals of their names;
- a script's component is the table `component` returned (`Door` in its own file), or its
  name (`e:has("Wallet")`).

`self:open()` calls your component's own function, `Door.open(self)`. The entity's methods
come first, so do not give your functions their names. The editor's types know your
functions only as `Door.open(self)`.

A `Transform` has methods too:

| Method | What |
|---|---|
| `t:rotate(axis, angle)` | turns it by `angle` radians about `axis`, in its parent's space |
| `t:look_at(point, up?)` | turns it so its forward points at `point` (in its parent's space), `up` (y by default) up |
| `t:forward()`, `t:right()`, `t:up()` | its −z, x and y, in its parent's space |

```luau
-- A turret: turns toward the player, its barrel a child named "Barrel".
local Turret = component("Turret", { reach = 20 })

function Turret.on_tick(self, dt: number)
    local player = world.find("Player")
    if not player then
        return
    end
    local t = self.Transform
    local to = player.Transform.translation - t.translation
    if vector.magnitude(to) < self.reach then
        t:look_at(player.Transform.translation)
        local barrel = self:find_child("Barrel")
        if barrel and not barrel:has(Trigger) then
            barrel:add(Trigger, { size = vector.create(0.2, 0.2, 2) })
        end
    end
end

return Turret
```

## The libraries

| Library | What |
|---|---|
| `world.find(name)` | the entity with that `Name` (the first made, of several), or `nil` |
| `time.now()`, `time.tick()`, `time.dt()` | the game's time in seconds, the tick's number, its length |
| `quat.identity()`, `quat.axis_angle(axis, angle)`, `quat.euler(x, y, z)` | rotations, in radians; `q * v` rotates a vector, `a * b` composes |
| `physics.raycast(origin, direction, max_distance, filter?)` | the first body along the ray: its entity (`nil` for a body without one), the point, the normal and the distance; `nil`s on a miss |
| `physics.overlap_sphere(center, radius, filter?)` | the entities whose bodies overlap the sphere, for a `for` loop |
| `physics.filter({ "Moving" })` | the collision layers a query sees, by name (`PhysicsSettings::layer_names`); make it once, at the module's top |
| `print(...)`, `warn(...)` | the engine's log, with the script's file and line |

Luau's own libraries (`math`, `string`, `table`, `vector`, `buffer`) are there too, in
Luau's sandbox.

`physics` queries the game's physics world (`Game::physics`), or the player's own: set
`PlayerSettings::physics` and the player runs one for a game that has none. It holds the
pack's bodies and the scene's models and brushes, and steps them after every tick
([physics.md](physics.md#the-players-own-physics-world)).

```luau
local Eye = component("Eye", { range = 20 })
local moving = physics.filter({ "Moving" })

function Eye.on_tick(self, dt: number)
    local t = self.Transform
    local e, point, normal, distance = physics.raycast(t.translation, vector.create(0, 0, -1), self.range, moving)
    if e then
        print("sees", e.name, "at", distance)
    end
end

return Eye
```

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
- an empty `on_tick` costs 0.14 µs per instance;
- reading and writing a prop or a component's field costs about 37 ns each (a number, a
  boolean, a vector, an entity);
- 1,000 doors like the one above take 0.6 ms a tick.

A field read is a call into the engine, though a short one: read it into a local when a
hook uses it several times. Logic over thousands of objects every tick belongs in Rust.

What the engine gives scripts allocates nothing while a game runs: entities, vectors and
numbers are values, and `self.Transform` or `world.find("Player").Transform` is made once
and given again. Your own tables, strings and closures do allocate, as in any Luau code.
A few things allocate by their nature:
- a `quat`, made or read from a component;
- `tostring`;
- a `physics.overlap_sphere` loop (its iterator);
- an enum field written by its name.

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

- `require("./wallet")` between your scripts;
- `world.spawn`, `world.query` and systems over queries;
- `input`;
- `send`, `after` timers, `audio`, `random`, `world.instantiate`;
- script components in saves.
