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
  - a number (`lift = 2.5`), a boolean (`open = false`), a vector (`closed =
    vector.zero`) or a string (`sound = "pickups/coin.ogg"`): their default and type;
  - `prop.number(default, { min, max, step, tooltip, hidden })`, `prop.integer(…)`,
    `prop.boolean(…)`, `prop.vector(…)`, `prop.string(…)`: the same, with limits and
    help. A write beyond `min` or `max` is clamped, as the inspector does;
  - `prop.entity()`: another entity, `nil` at first.

  A string prop is the component's own text: saved, kept by scenes and undo, edited in
  the inspector. Reading the same text again makes nothing, and writing one reuses its
  room. A world holding string props cannot be snapshotted whole, as one holding sounds
  cannot.
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
| `on_load(self)` | a save was applied: your props hold the saved values (at the next tick, after `on_start` for an instance the load made) |
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

## Messages and timers

```luau
-- Tell the door's scripts: every one with an `open` function runs it.
world.find("Door"):send("open", "key", self.entity)

-- Run this script's `explode` in two seconds of game time.
self:after(2, "explode", 3)

function Crate.explode(self, power)
    -- …
end
```

- `e:send(hook, …)` calls `hook` on each script of `e` that has a function of that name,
  after the hook that sends returns. `self:send` sends to your own entity.
- `self:after(seconds, hook, …)` calls `hook` on this script later, at the start of a tick.
- Both carry at most four values: nil, booleans, numbers, strings, vectors, entities. A
  table stays where it is: send what it holds, or keep it in `self.locals`.
- A message to an entity that is gone is dropped. One that no script of the entity
  hears is said once in the log, as is a loop of messages that never ends (it stops at
  16 rounds a stage, the rest go on next tick).

## Many objects at once: queries and systems

A hook runs once per object. For many alike objects, a system runs once a tick and loops
over a query:

```luau
-- scripts/spin.luau: everything with Spin turns about its up axis.
local Spin = component("Spin", { speed = math.rad(90) })

local spinning = world.query(Spin, Transform) -- made once, at the top
local UP = vector.create(0, 1, 0)

system("Spin", {
    on_tick = function(dt: number)
        for e, spin, t in spinning do
            t:rotate(UP, spin.speed * dt)
        end
    end,
})

return Spin
```

- `world.query(A, B, …)` gives each entity that has all of them, with a view of each:
  `for e, a, b in q do`. `q:with(C)` keeps only those that also have `C`,
  `q:without(D)` those without `D`.
- A loop visits what matched when it began; an entity that goes or loses a component
  during it is passed over.
- The views a loop gives are the query's own, moved to each entity in turn: keep the
  entity (`e`), not the view, past the step.
- Make a query once, at the script's top: making one inside a hook allocates.
- `system(name, { on_tick = f, on_frame = g })`: `on_tick(dt)` runs once a tick, after
  the components' hooks, `on_frame(dt, alpha)` once a frame; systems run in the order
  declared.

`world.instantiate` places a prefab, any scene of your game, at once:

```luau
local Spawner = component("Spawner", { crate = prop.asset("scene") }) -- set in the inspector

function Spawner.on_tick(self, dt: number)
    if input.just_pressed("drop") and self.crate then
        local made = world.instantiate(self.crate, self.Transform.translation)
        made:find_child("Lid").Transform.translation += vector.create(0, 0.5, 0)
    end
end
```

- `prefab` is an asset (`prop.asset`, dropped on the field in the inspector) or the
  scene's path (`"props/crate.scene"`).
- Its roots move by the vector given; the root's entity comes back (a scene of several
  roots gets one more entity holding them).
- What scripts place is theirs alone: no scene id, so saving and loading keep each
  instance apart.

`world.spawn` makes an entity at once:

```luau
local bullet = world.spawn({
    Name = "Bullet", -- a component of one field takes its value
    Transform = { translation = muzzle },
    Bullet = { speed = 40 }, -- a script's component: its hooks start next tick
})
bullet.Transform.translation += vector.create(0, 0.1, 0) -- there already
```

## Multiplayer

A script's state is its props: they are saved, rolled back and, once your network schema
names them, replicated. Keep `self.locals` for caches.

Name a script's component in your game's schema (`docs/dev-guide/networking.md`):

```rust
fn schema() -> NetSchema {
    NetSchema::new(MY_PROTOCOL)
        .replicate::<Player>()
        .replicate_script("Door") // found when the session starts, once scripts have loaded
}
```

- Its props go with the precision you give them: `prop.number(0, { net_quantum = 0.01 })`
  sends hundredths, `net_skip = true` keeps one local.
- String props stay local.
- A client's copy starts from your script's defaults: what is not sent keeps them.

- `net.is_server()`: this side decides the world (playing alone, or hosting);
  `net.is_client()`: it plays on someone else's host.
- On a client, only your own predicted entities' tick hooks run. They run again when the
  host corrects a prediction, and `net.resimulating()` says so. While they do, sounds,
  messages and timers are left out: they started the first time.
- Systems, timers and contacts run on the host.
- Use `time` and `random` for anything the game's state depends on: they are the same on
  every machine, and kept by saves and rollback. `os.clock` and `math.random` are not.

## Scripts that use each other

`require` gives you another script by its path from yours, as luau-lsp finds it:

```luau
-- scripts/coin.luau
local Wallet = require("./wallet") -- scripts/wallet.luau
local maths = require("@shared/maths") -- an alias of a .luaurc
```

- What you get is what that script returned: its component's table (for
  `e:has(Wallet)`), or a table of functions.
- `../` goes up a folder (`require("../shared/maths")` from `scripts/npc/guard.luau`), and
  `scripts/npc/init.luau` is what `require("./npc")` finds.
- Aliases come from a `.luaurc` in your game's folder (or in `scripts/` and below), as
  luau-lsp reads them: `{ "aliases": { "shared": "./scripts/shared" } }`.
- A script runs after the scripts it requires, whatever their files are called. Two that
  require each other cannot both run first: the log says so.
- When you save a script, the ones that require it run again after it, so none keeps the
  old one.

## The libraries

| Library | What |
|---|---|
| `world.find(name)` | the entity with that `Name` (the first made, of several), or `nil` |
| `input.pressed(a)`, `input.just_pressed(a)`, `input.just_released(a)` | your game's action `a` (its input map): held, gone down, gone up |
| `input.strength(a)`, `input.axis(a)`, `input.vector(a)` | how far it is held (0 to 1), a 1D axis (−1 to 1), a 2D axis as a vector (x, y, 0) |
| `random.number()`, `random.integer(a, b)`, `random.vector_in_sphere()` | the world's random numbers: in [0, 1), a whole number from `a` to `b` (both included), a point inside the sphere of radius 1. Seeded the same on every machine, kept by saves and network rollback; `math.random` is not |
| `audio.play(sound, bus?)`, `audio.play_at(sound, position, bus?)` | a sound played once, as it is (bus `ui`) or at a place in the world (bus `effects`): an entity that despawns at the sound's end |
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

## Breakpoints, logpoints and pause points

In a script's tab, the column left of the line numbers holds breakpoints. They live in the
editor's session (`.ofelia/`), never in the script, and follow their lines as you edit.

- **A breakpoint** (●) stops the script at its line during Play: click the column, or F9
  on the caret's line. The game waits in the middle of its tick (the hooks before this one
  ran, the rest did not), and the editor goes on:
  - the script's tab comes to the front at the line, with an amber band and an arrow, and
    the frame's values after it (`self = Self   dt = 0.015625`): its locals, then the
    upvalues its function uses;
  - the viewport's edge turns amber, the mouse is the editor's, and the status bar says
    where (`Stopped at scripts/door.luau:14 in on_tick`);
  - the world is yours meanwhile: select, inspect, edit (as play edits, put back at Stop);
    nothing in it moves but by your edits. Breakpoints can be set, changed or taken away;
  - **F5 goes on**: the rest of the tick runs, the game's own tick and the physics step
    with it. **F8 stops Play** there: the stopped hook ends at its line, without an error.
  - **Steps:** F10 runs to the next line of the function (over the calls it makes), F11
    to the next line that runs (into a function it calls), Shift+F11 to the line the
    function returns to. A step past a hook's end stops at the next script line that runs,
    in the next hook. Each step's stop shows its line and its values, as a breakpoint's.
  - A stop inside a call from C code (a `table.sort` comparison, a metamethod written in
    Luau) cannot be stepped from: F5 goes on there. `self:method()` can.
  - **The Debugger panel** (a bottom tab, Ctrl+Shift+D) opens at a stop: Continue and the
    steps as buttons, the call stack (a click on a caller shows its line and its values),
    the shown frame's locals and upvalues, and every breakpoint with what it does and its
    hits (a click on its mark turns it off or on, on its row goes to its line).
  - **The header** says what stopped, in which hook, for which entity, where, and in which
    tick: `Stopped · on_tick · Front door · scripts/door.luau:14 · tick 1,284, under way`
    (the hooks before this one ran, the rest wait).
  - **Shift+F7 finishes the tick** (or Finish tick in the header): the rest of the tick
    runs, its stops passed, then Play is paused, a whole world with every hook of the tick
    done. F7 goes on from there.
  - **"Only Front door"** in the header limits the breakpoint that stopped to that
    entity: the next instances of the script pass it. Its row says `only Front door`, its
    mark shows a bar, and the header's button reads "Every instance" to take the filter
    away. A scene entity is kept by its scene id in the session; one the game made in Play
    is kept until Play stops.
  - **Values in the code:** the lines above the stopped one, in its function, show what
    they assigned at their ends (`t = Transform`, `self.speed = 2`), dimmer than the code;
    the function's line has its parameters. A value that changed since the last stop is
    lit, there and in the Variables.
  - **The Variables open:** `self` shows its props, its entity's other components and the
    entity; an entity shows its name and its components, a component its fields, a table
    its fields (the first hundred, then how many more). A click opens or closes a row.
  - **Evaluate** (the line under the Variables): a Luau expression in the shown frame, its
    locals, upvalues and the module's globals in reach (`self.speed * 2`,
    `t.translation.y`); its answer stands above the line. **Ctrl+Enter makes it a watch**,
    evaluated again at every stop, first in the Variables, kept with the project (an ×
    takes it away). It runs under the scripts' time budget; breakpoints it reaches pass.
  - A script saved while stopped runs again once it goes on.
- **Errors stop a script too** while Play runs, before its frames unwind: an error the
  script does not catch stops it at the failing line, the line in red with the error's
  message after it, the viewport's edge red, the Debugger panel open on its stack.
  - **F5 lets the error take its course:** the instance stops, as it does without the
    debugger, and the Output has its row. **F8 stops Play.** There are no steps from an
    error.
  - **Once a line a Play:** other errors at that line (the same bug in a hundred coins)
    go to the Output without stopping.
  - The time budget's error (a loop that never ends) stops where the time ran out, inside
    the loop.
  - The Debugger's Breakpoints list begins with which errors stop: **Errors the script
    does not catch** (on) and **Errors caught by pcall** (off), each a click (or the
    palette's Debug commands) away. Kept in the session.
  - An error inside a function running as Luau's native code shows its line, its message
    and the stack, but not that frame's locals: Luau does not read them. A breakpoint in
    the function runs it interpreted, and the next stop there shows them.
  - **Full script debugging** (the Breakpoints list's third row, or the palette's Debug:
    Full Script Debugging) runs every script interpreted from the next Play, so a stop
    anywhere shows every frame's locals. The scripts run slower meanwhile (8–33 % on the
    engine's script scenes). Off, the default, they run as native code, as the game runs
    them. Kept in the session.
- **Break at the Next Script Line** (the palette): the next line any script runs stops,
  as a breakpoint does, wherever it is.
- **A logpoint** (◆) writes a message each time the line runs, and the script goes on:
  Shift+click the column, or Shift+F9, and type its text. `{…}` takes any Luau expression
  in the line's scope, as Luau's own string interpolation does: `speed = {self.speed}, near
  = {near}`. While Play runs, the latest message stands after its line with how many times
  it came (`speed = 2  ×312`), and the Output has a row each time, its place a click away.
- **A pause point** (‖) pauses Play after the tick that reaches it, as F7 does: the world
  is still, every hook of that tick done. Make one from a breakpoint's options. F7 or F5
  goes on.
- **Options** (Alt+F9, or a right click): what it does (Stop, Log or Pause; a click on the
  switch goes to the next), a **condition** (`self.hp < 0`) that limits it to when it holds,
  a **hit rule** for some of its hits: `10` the tenth, `>= 10` from the tenth on, `% 64`
  every 64th (once a second at 64 ticks). Tab moves between the fields, Enter keeps,
  Escape drops. A mark with a condition or a rule has a bar in it.
- Alt+click or Ctrl+F9 turns one off (its mark hollow) or on; a middle click removes it;
  Ctrl+Shift+F9 removes them all ("Restore Removed Breakpoints" in the palette brings them
  back).
- **A blank or a comment's line** has no code: its breakpoint moves to the next line with
  code. One that cannot be placed, or whose condition or text fails, says why after its
  line, with a dot by its mark.
- **Unsaved edits** move the marks, not the code that runs: the game keeps its breakpoints
  on its lines until you save, and one set on a line you just wrote waits for the save.

**Changing a value at a stop:** F2 over a row of the Variables, or a second click on it,
opens its value as Luau; Enter gives it (`dt * 4`, `vector.create(0, 1, 0)`, `"open"`),
Escape drops it. The stop is read again, so what changed shows wherever it is.

- A local or an upvalue changes as it is; so does a field of a table it holds.
- `self`'s props and its entity's components (`self.speed`, `self.Transform.translation`)
  are written through the script's own view, as one undo step: Ctrl+Z puts them back, and
  Stop puts them back unless kept with Ctrl+K, as the inspector's edits in Play.
- A value the compiler folded into a constant changes where it is kept, not in the code
  that runs: a local never written anywhere (`local BASE = 10`), and a field of a local
  table that is never written and never leaves its function. As in any compiled language.
- A native frame's locals cannot be written (they are not shown either).

Without breakpoints nothing is paid. A function with one runs Luau's interpreter instead of
native code until its script runs again, and each hit costs a microsecond or so (below).

### Debugging from VS Code

The editor is also a debug adapter (DAP): VS Code debugs the scripts it plays, with the same
breakpoints as the editor's gutter.

- **Set up:** `ofeliaengine new` writes `.vscode/launch.json` and installs the ofeliaengine
  extension where VS Code's `code` command is on the path; its `.vsix` is in `.vscode/`
  otherwise (Install from VSIX in VS Code's Extensions view).
- **Attach:** start the editor on the game, open the game's folder in VS Code, and press F5
  with **Attach to the editor**. **Attach and Play** also plays the open scene, and stopping
  the session (Shift+F5) stops Play.
- **What works:** breakpoints (conditions, hit counts, logpoints) set in either, the two
  error filters ("Errors the script does not catch", "Errors caught by pcall"), the stack,
  Locals and Upvalues (`self` opens into its props and its entity's components), hovers,
  watches and the debug console (evaluated in the frame shown), Set Value on a variable (as
  F2 in the editor, `self`'s an undo step), F5, F10, F11, Shift+F11 and Pause (the next line
  a script runs stops). Logpoints' messages come to the debug console.
- **One set of breakpoints:** those set in VS Code become the editor's, and those set in the
  editor show in VS Code as they are made, moved or taken away. Attaching joins the two sets.
  VS Code shows an editor breakpoint by its line only (it keeps no options for breakpoints a
  debugger makes): its condition, hit count, log text and entity filter stay the editor's,
  and VS Code's own list for the file does not take them away.
- **Safety:** the editor listens on 127.0.0.1 only, on a port it picks, and writes it with a
  token to `.ofelia/debug/editor.json`; the extension gives the token, and nothing without it
  attaches. One client at a time.

### Debugging the game outside the editor

A development build of the game (`cargo run`) is a debug adapter too, when you ask:

- **Start it** with `cargo run -- --debug`. With `cargo run -- --wait-for-debugger` its world
  waits, still, until VS Code has attached and set its breakpoints, so the scripts' first
  hooks can stop.
- **Attach:** F5 in VS Code with **Attach to the player** (`ofeliaengine new` writes it into
  `launch.json`). With several players of the game running, pick one from the list.
- **What works:** everything above but Play: breakpoints, logpoints, the error filters, the
  stack, the values, hovers, watches, the debug console, the steps and Pause.
- **At a stop** the world is kept still and still drawn; the window's title says where
  ("stopped at scripts/door.luau:12", or "an error at"), and the mouse is freed until it goes
  on.
- **When VS Code goes,** its breakpoints and error stops go with it, and a stop goes on: the
  game has no breakpoints of its own.
- **Release and shipping builds have no debugger:** `--debug` there says so in the log and
  does nothing. Each player writes its port and token to `.ofelia/debug/player-<pid>.json`
  and takes the file away when it closes.

`cargo run -- --debug` compiles the scripts to be debugged, as the editor does.

Stopping at errors costs nothing until an error comes: hooks are called with a message
handler, as they were with mlua's own, 14 ns a call more at most (below). "Errors caught by
pcall" runs hooks on the debugger's thread, 0.12 µs a hook, only while it is on.

In the editor, the scripts are compiled to be debugged: Luau's optimization level 1, which
inlines nothing, so a step goes into a local function as its text reads. It costs nothing
measurable (below); a game outside the editor runs level 2, unless started with `--debug`.

## What it costs

Measured on ref-1 (Ryzen 7 7700, release):
- an empty `on_tick` costs 0.14 µs per instance;
- 10,000 spinners take 1.2 ms a tick as a system, 2.7 ms as a hook each;
- reading and writing a prop or a component's field costs about 37 ns each (a number, a
  boolean, a vector, an entity);
- 1,000 doors like the one above take 0.6 ms a tick;
- a breakpoint turned off costs nothing; one on a line that runs costs about 0.6 µs a hit
  (one in 64 logging), 1 µs with a condition, 1.4 µs logging every hit;
- in the editor's Play, the scripts' stages run on a stack of their own so that a
  breakpoint can stop them: 1,000 spinners tick in 0.26 ms either way;
- while a breakpoint that stops is set, each hook runs on a Luau thread of the debugger's,
  which can be stepped: 0.12 µs more a hook (1,000 doors 0.59 -> 0.79 ms a tick); without
  one, nothing.

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

### Measuring your scripts

**The editor's profiler** (the bottom bar's Profiler tab, "View: Profiler") lists the
game's scripts while it plays, the costliest first, under **Scripts per frame**: each
component and system with its hooks' time a frame, their calls a frame and the memory it
holds, averaged over the last half second (`Door  12.5 µs · 40 calls · 3.2 KB`).
- **The time** is the hooks' whole time, the engine's calls they make included. The time
  a breakpoint held one stopped is left out.
- **The memory** is what its hooks allocated since the panel opened that the collector has
  not freed yet. Garbage counts until it is collected, so a script that makes a table or a
  `quat` every tick shows a saw. What its module made when it loaded is not counted.
- Up to 254 scripts are told apart; those past them share the last count.
- Nothing is measured while the panel is closed; a hook then pays one check for it. While
  it is open, each hook costs about 0.07 µs more.

**Tracy** (the profiling build, [profiling.md](profiling.md)) has a zone for every call of
a hook, named after its component and hook: `script Door.on_tick`, `script Door.open` for a
message's hook, `script SpinSystem.on_tick` for a system's.

## Types: in the editor and in VS Code

The editor knows the engine's API and your components: it underlines a wrong field or a
value of the wrong type as you write, completes `self.Transform.` and shows types on
hover. In a hook, `self` is your component's props with the entity's components
(`self.lift`, `self.Transform.translation`), typed from `component`'s declaration: write
`function Door.on_tick(self, dt: number)` and nothing more. To name that type yourself,
`type Self = Script<typeof(Door.props)>` gives it (`function Door.on_tick(self: Self, dt:
number)`).

The editor writes the same types to `types/ofelia.d.luau` in your game's folder whenever
they change (a script declared, a component added). A game made with `ofeliaengine new`
has `.vscode/settings.json` pointing [luau-lsp](https://github.com/JohnnyMorganz/luau-lsp)
at it, so VS Code checks and completes your scripts the same way. The file is in
`.gitignore`: open the editor once after you clone to have it.

- A field named by one of Luau's reserved words (`Timer.repeat`) has no type: read it as
  `timer["repeat"]`.
- Lists, maps and asset references are not readable from scripts yet, and have no types.

`input` reads the tick's view in a tick's hooks (`on_tick`, `on_start`, contacts, systems'
`on_tick`), where a tap shorter than a tick still counts, and the frame's in `on_frame`.
Move what the game's state depends on in ticks; turn a camera in frames:

```luau
function Walker.on_tick(self, dt: number)
    local move = input.vector("move")
    local speed = if input.pressed("run") then self.speed * 2 else self.speed
    local t = self.Transform
    t.translation += (t:right() * move.x + t:forward() * move.y) * (speed * dt)
end

function Walker.on_frame(self, dt: number)
    local camera = self:find_child("Camera")
    if camera then
        camera.Transform:rotate(vector.create(1, 0, 0), -input.vector("look").y * 0.002)
    end
end
```

## Coming next

- scripts, scenes and the prefabs scripts place read from a release's packs (with the
  one-button build).
